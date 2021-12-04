//! `tiny-lsm` is a dead-simple in-memory LSM for managing
//! fixed-size metadata in more complex systems.
//!
//! Uses crc32fast to checksum all key-value pairs in the log and
//! sstables. Uses zstd to compress all sstables. Performs sstable
//! compaction in the background.
//!
//! Because the data is in-memory, there is no need to put bloom
//! filters on the sstables, and read operations cannot fail due
//! to IO issues.
//!
//! `Lsm` implements `Deref<Target=BTree<[u8; K], [u8; V]>>`
//! to immutably access the data directly without any IO or
//! blocking.
//!
//! `Lsm::insert` writes all data into a 32-kb `BufWriter`
//! in front of a log file, so it will block for very
//! short periods of time here and there. SST compaction
//! is handled completely in the background.
//!
//! This is a bad choice for large data sets if you
//! require quick recovery time because it needs to read all of
//! the sstables and the write ahead log when starting up.
//!
//! The benefit to using tiered sstables at all, despite being
//! in-memory, is that they act as an effective log-deduplication
//! mechanism, keeping space amplification very low.
//!
//! Maximum throughput is not the goal of this project. Low space
//! amplification and very simple code is the goal, because this
//! is intended to maintain metadata in more complex systems.
//!
//! There is currently no compaction throttling. You can play
//! with the constants around compaction to change compaction
//! characteristics.
//!
//! Never change the constant size of keys or values for an existing
//! database.
//!
//! Basically untested, but on the flip side there are less
//! than 500 lines of bugs. Let it be a case study for how
//! many bugs can exist in a codebase with 0 usage of unsafe.
//!
//! # Examples
//!
//! ```
//! // open up the LSM
//! let mut lsm = tiny_lsm::Lsm::recover("path/to/base/dir").expect("recover lsm");
//!
//! // store some things
//! let key: [u8; 8] = 8_u64.to_le_bytes();
//! let value: [u8; 1] = 255_u8.to_le_bytes();
//! lsm.insert(key, value);
//!
//! assert_eq!(lsm.get(&key), Some(&value));
//!
//! ```
#![cfg_attr(test, feature(no_coverage))]

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, prelude::*, BufReader, BufWriter, Result};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

const SSTABLE_DIR: &str = "sstables";

#[derive(Debug, Clone, Copy)]
#[cfg_attr(
    test,
    derive(serde::Serialize, serde::Deserialize, fuzzcheck::DefaultMutator)
)]
pub struct Config {
    /// If on-disk uncompressed sstable data exceeds in-memory usage
    /// by this proportion, a full-compaction of all sstables will
    /// occur. This is only likely to happen in situations where
    /// multiple versions of most of the database's keys exist
    /// in multiple sstables, but should never happen for workloads
    /// where mostly new keys are being written.
    pub max_space_amp: u64,
    /// When the log file exceeds this size, a new compressed
    /// and compacted sstable will be flushed to disk and the
    /// log file will be truncated.
    pub max_log_length: usize,
    /// When the background compactor thread looks for contiguous
    /// ranges of sstables to merge, it will require all sstables
    /// to be at least 1/`merge_ratio` * the size of the first sstable
    /// in the contiguous window under consideration.
    pub merge_ratio: u64,
    /// When the background compactor thread looks for ranges of
    /// sstables to merge, it will require ranges to be at least
    /// this long.
    pub merge_window: usize,
    /// All inserts go directly to a `BufWriter` wrapping the log
    /// file. This option determines how large that in-memory buffer
    /// is.
    pub log_bufwriter_size: usize,
    /// The level of compression to use for the sstables with zstd.
    pub zstd_sstable_compression_level: u8,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            max_space_amp: 2,
            max_log_length: 32 * 1024 * 1024,
            merge_ratio: 3,
            merge_window: 10,
            log_bufwriter_size: 32 * 1024,
            zstd_sstable_compression_level: 3,
        }
    }
}

fn hash<const K: usize, const V: usize>(k: &[u8; K], v: &Option<[u8; V]>) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&[v.is_some() as u8]);
    hasher.update(&*k);

    if let Some(v) = v {
        hasher.update(v);
    } else {
        hasher.update(&[0; V]);
    }

    // we XOR the hash to make sure it's something other than 0 when empty,
    // because 0 is an easy value to create accidentally or via corruption.
    hasher.finalize() ^ 0xFF
}

enum WorkerMessage {
    NewSST { id: u64, sst_sz: u64, db_sz: u64 },
    Stop(mpsc::Sender<()>),
    Heartbeat(mpsc::Sender<()>),
}

struct Worker<const K: usize, const V: usize> {
    sstable_directory: BTreeMap<u64, u64>,
    inbox: mpsc::Receiver<WorkerMessage>,
    db_sz: u64,
    path: PathBuf,
    config: Config,
}

impl<const K: usize, const V: usize> Worker<K, V> {
    fn run(mut self) {
        loop {
            match self.inbox.try_recv() {
                Ok(WorkerMessage::NewSST { id, sst_sz, db_sz }) => {
                    self.db_sz = db_sz;
                    self.sstable_directory.insert(id, sst_sz);
                }
                Ok(WorkerMessage::Stop(dropper)) => {
                    drop(dropper);
                    return;
                }
                Ok(WorkerMessage::Heartbeat(dropper)) => {
                    drop(dropper);
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    log::info!("tiny-lsm compaction worker quitting");
                    return;
                }
            };

            let res = self.sstable_maintenance();
            match res {
                Ok(false) => {}
                Ok(true) => continue,
                Err(e) => {
                    log::error!("error while compacting sstables in the background: {:?}", e);
                }
            }

            // no work to do, wait for new sstables
            match self.inbox.recv() {
                Ok(WorkerMessage::NewSST { id, sst_sz, db_sz }) => {
                    self.db_sz = db_sz;
                    self.sstable_directory.insert(id, sst_sz);
                }
                Ok(WorkerMessage::Stop(dropper)) => {
                    drop(dropper);
                    log::info!("tiny-lsm compaction worker quitting");
                    return;
                }
                Ok(WorkerMessage::Heartbeat(dropper)) => {
                    drop(dropper);
                }
                Err(mpsc::RecvError) => {
                    log::info!("tiny-lsm compaction worker quitting");
                    return;
                }
            }
        }
    }

    fn sstable_maintenance(&mut self) -> Result<bool> {
        let on_disk_size: u64 = self.sstable_directory.values().sum();

        log::debug!("disk size: {} mem size: {}", on_disk_size, self.db_sz);
        if self.sstable_directory.len() > 1 && on_disk_size / self.db_sz > self.config.max_space_amp
        {
            log::info!(
                "performing full compaction, decompressed on-disk \
                database size has grown beyond {}x the in-memory size",
                self.config.max_space_amp
            );
            let run_to_compact: Vec<u64> = self.sstable_directory.keys().copied().collect();

            self.compact_sstable_run(&run_to_compact)?;
            return Ok(true);
        }

        if self.sstable_directory.len() < self.config.merge_window {
            return Ok(false);
        }

        for window in self
            .sstable_directory
            .iter()
            .collect::<Vec<_>>()
            .windows(self.config.merge_window)
        {
            if window
                .iter()
                .skip(1)
                .all(|w| *w.1 * self.config.merge_ratio > *window[0].1)
            {
                let run_to_compact: Vec<u64> = window.into_iter().map(|(id, _sum)| **id).collect();

                self.compact_sstable_run(&run_to_compact)?;
                return Ok(true);
            }
        }

        Ok(false)
    }

    fn compact_sstable_run(&mut self, sstable_ids: &[u64]) -> Result<()> {
        log::debug!(
            "trying to compact sstable_ids {:?}",
            sstable_ids
                .iter()
                .map(|id| id_format(*id))
                .collect::<Vec<_>>()
        );

        let mut map = BTreeMap::new();

        for sstable_id in sstable_ids {
            for (k, v) in read_sstable::<K, V>(&self.path, *sstable_id)? {
                map.insert(k, v);
            }
        }

        let sst_id = sstable_ids
            .iter()
            .max()
            .expect("compact_sstable_run called with empty set of sst ids");

        write_sstable(&self.path, *sst_id, &map, true, &self.config)?;

        let sst_sz = map.len() as u64 * (4 + K + V) as u64;
        self.sstable_directory.insert(*sst_id, sst_sz);

        log::debug!("compacted range into sstable {}", id_format(*sst_id));

        for sstable_id in sstable_ids {
            if sstable_id == sst_id {
                continue;
            }
            fs::remove_file(self.path.join(SSTABLE_DIR).join(id_format(*sstable_id)))?;
            self.sstable_directory
                .remove(sstable_id)
                .expect("compacted sst not present in sstable_directory");
        }

        fs::File::open(self.path.join(SSTABLE_DIR))?.sync_all()?;

        Ok(())
    }
}

fn id_format(id: u64) -> String {
    format!("{:016x}", id)
}

fn list_sstables(path: &Path) -> Result<BTreeMap<u64, u64>> {
    let mut sstable_map = BTreeMap::new();

    for dir_entry_res in fs::read_dir(path.join(SSTABLE_DIR))? {
        let dir_entry = dir_entry_res?;
        let file_name = if let Ok(f) = dir_entry.file_name().into_string() {
            f
        } else {
            continue;
        };

        if let Ok(id) = u64::from_str_radix(&file_name, 16) {
            let metadata = dir_entry.metadata()?;

            sstable_map.insert(id, metadata.len());
        } else {
            if file_name.ends_with("-tmp") {
                log::warn!("removing incomplete sstable rewrite {}", file_name);
                fs::remove_file(path.join(SSTABLE_DIR).join(file_name))?;
            }
        }
    }

    Ok(sstable_map)
}

fn write_sstable<const K: usize, const V: usize>(
    path: &Path,
    id: u64,
    items: &BTreeMap<[u8; K], Option<[u8; V]>>,
    tmp_mv: bool,
    config: &Config,
) -> Result<()> {
    let sst_dir_path = path.join(SSTABLE_DIR);
    let sst_path = if tmp_mv {
        sst_dir_path.join(format!("{:x}-tmp", id))
    } else {
        sst_dir_path.join(id_format(id))
    };

    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&sst_path)?;

    let mut bw = BufWriter::new(
        zstd::Encoder::new(file, config.zstd_sstable_compression_level as _)
            .expect("zstd encoder failure"),
    );

    bw.write(&(items.len() as u64).to_le_bytes())?;

    for (k, v) in items {
        let crc: u32 = hash(k, v);
        bw.write(&crc.to_le_bytes())?;
        bw.write(&[v.is_some() as u8])?;
        bw.write(k)?;

        if let Some(v) = v {
            bw.write(v)?;
        } else {
            bw.write(&[0; V])?;
        }
    }

    bw.flush()?;

    bw.get_mut().get_mut().sync_all()?;
    fs::File::open(path.join(SSTABLE_DIR))?.sync_all()?;

    if tmp_mv {
        let new_path = sst_dir_path.join(id_format(id));
        fs::rename(sst_path, new_path)?;
    }

    Ok(())
}

fn read_sstable<const K: usize, const V: usize>(
    path: &Path,
    id: u64,
) -> Result<Vec<([u8; K], Option<[u8; V]>)>> {
    let file = fs::OpenOptions::new()
        .read(true)
        .open(path.join(SSTABLE_DIR).join(id_format(id)))?;

    let mut reader = zstd::Decoder::new(BufReader::with_capacity(16 * 1024 * 1024, file)).unwrap();

    // crc + tombstone discriminant + key + value
    let mut buf = vec![0; 4 + 1 + K + V];

    let len_buf = &mut [0; 8];

    reader.read_exact(len_buf)?;

    let expected_len: u64 = u64::from_le_bytes(*len_buf);
    let mut sstable = Vec::with_capacity(expected_len as usize);

    while let Ok(()) = reader.read_exact(&mut buf) {
        let crc_expected: u32 = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        let d: bool = match buf[4] {
            0 => false,
            1 => true,
            _ => {
                log::warn!("detected torn-write while reading sstable {:016x}", id);
                break;
            }
        };
        let k: [u8; K] = buf[5..K + 5].try_into().unwrap();
        let v: Option<[u8; V]> = if d {
            Some(buf[K + 5..5 + K + V].try_into().unwrap())
        } else {
            None
        };
        let crc_actual: u32 = hash(&k, &v);

        if crc_expected != crc_actual {
            log::warn!("detected torn-write while reading sstable {:016x}", id);
            break;
        }

        sstable.push((k, v));
    }

    if sstable.len() as u64 != expected_len {
        log::warn!(
            "sstable {:016x} tear detected - process probably crashed \
            before full sstable could be written out",
            id
        );
    }

    Ok(sstable)
}

pub struct Lsm<const K: usize, const V: usize> {
    // `BufWriter` flushes on drop
    log: BufWriter<fs::File>,
    memtable: BTreeMap<[u8; K], Option<[u8; V]>>,
    db: BTreeMap<[u8; K], [u8; V]>,
    worker_outbox: mpsc::Sender<WorkerMessage>,
    next_sstable_id: u64,
    dirty_bytes: usize,
    path: PathBuf,
    config: Config,
}

impl<const K: usize, const V: usize> Drop for Lsm<K, V> {
    fn drop(&mut self) {
        if let Err(e) = self.flush() {
            log::error!("failed to flush while dropping Lsm: {:?}", e);
        }

        let (tx, rx) = mpsc::channel();

        if self.worker_outbox.send(WorkerMessage::Stop(tx)).is_err() {
            log::error!("failed to shut down compaction worker on Lsm drop");
            return;
        }

        for _ in rx {}
    }
}

impl<const K: usize, const V: usize> std::ops::Deref for Lsm<K, V> {
    type Target = BTreeMap<[u8; K], [u8; V]>;

    fn deref(&self) -> &Self::Target {
        &self.db
    }
}

impl<const K: usize, const V: usize> Lsm<K, V> {
    /// Recover the LSM off disk. Make sure to never
    /// recover a DB using different K, V parameters than
    /// it was created with, or there may be data loss.
    ///
    /// This is an O(N) operation and involves reading
    /// all previously written sstables and the log,
    /// to recover all data into an in-memory `BTreeMap`.
    pub fn recover<P: AsRef<Path>>(p: P) -> Result<Lsm<K, V>> {
        Lsm::recover_with_config(p, Config::default())
    }

    /// Recover the LSM, and provide custom options
    /// around IO and merging. All values in the `Config`
    /// object are safe to change across restarts, unlike
    /// the fixed K and V lengths for data in the database.
    pub fn recover_with_config<P: AsRef<Path>>(p: P, config: Config) -> Result<Lsm<K, V>> {
        let path = p.as_ref();
        if !path.exists() {
            fs::create_dir_all(path)?;
            fs::create_dir(path.join(SSTABLE_DIR))?;
            fs::File::open(path.join(SSTABLE_DIR))?.sync_all()?;
            fs::File::open(path)?.sync_all()?;
            let mut parent_opt = path.parent();

            // need to recursively fsync parents since
            // we used create_dir_all
            while let Some(parent) = parent_opt {
                if parent.file_name().is_none() {
                    break;
                }
                if fs::File::open(parent).and_then(|f| f.sync_all()).is_err() {
                    // we made a reasonable attempt, but permissions
                    // can sometimes get in the way, and at this point it's
                    // becoming pedantic.
                    break;
                }
                parent_opt = parent.parent();
            }
        }

        let log = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path.join("log"))?;

        let sstable_directory = list_sstables(path)?;

        let mut db = BTreeMap::new();
        for sstable_id in sstable_directory.keys() {
            for (k, v) in read_sstable::<K, V>(path, *sstable_id)? {
                if let Some(v) = v {
                    db.insert(k, v);
                } else {
                    db.remove(&k);
                }
            }
        }

        let max_sstable_id = sstable_directory.keys().next_back().copied();

        let mut reader = BufReader::new(log);

        let mut buf = vec![0; 5 + K + V];

        let mut memtable = BTreeMap::new();
        let mut recovered = 0;
        while let Ok(()) = reader.read_exact(&mut buf) {
            let crc_expected: u32 = u32::from_le_bytes(buf[0..4].try_into().unwrap());
            let d: bool = match buf[4] {
                0 => false,
                1 => true,
                _ => {
                    // need to back up a few bytes to chop off the torn log
                    let log_file = reader.get_mut();
                    log_file.seek(io::SeekFrom::Start(recovered))?;
                    log_file.set_len(recovered)?;
                    log_file.sync_all()?;
                    fs::File::open(path.join(SSTABLE_DIR))?.sync_all()?;
                    break;
                }
            };
            let k: [u8; K] = buf[5..K + 5].try_into().unwrap();
            let v: Option<[u8; V]> = if d {
                Some(buf[K + 5..5 + K + V].try_into().unwrap())
            } else {
                None
            };
            let crc_actual: u32 = hash(&k, &v);

            if crc_expected != crc_actual {
                // need to back up a few bytes to chop off the torn log
                let log_file = reader.get_mut();
                log_file.seek(io::SeekFrom::Start(recovered))?;
                log_file.set_len(recovered)?;
                log_file.sync_all()?;
                fs::File::open(path.join(SSTABLE_DIR))?.sync_all()?;
                break;
            }

            recovered += buf.len() as u64;

            memtable.insert(k, v);

            if let Some(v) = v {
                db.insert(k, v);
            } else {
                db.remove(&k);
            }
        }

        let (tx, rx) = mpsc::channel();

        let worker: Worker<K, V> = Worker {
            path: path.clone().into(),
            sstable_directory,
            inbox: rx,
            db_sz: db.len() as u64 * (K + V) as u64,
            config,
        };

        std::thread::spawn(move || worker.run());
        let (hb_tx, hb_rx) = mpsc::channel();
        tx.send(WorkerMessage::Heartbeat(hb_tx)).unwrap();
        for _ in hb_rx {}

        let lsm = Lsm {
            log: BufWriter::with_capacity(config.log_bufwriter_size, reader.into_inner()),
            memtable,
            db,
            path: path.into(),
            next_sstable_id: max_sstable_id.unwrap_or(0) + 1,
            dirty_bytes: recovered as usize,
            worker_outbox: tx,
            config,
        };

        Ok(lsm)
    }

    /// Writes a KV pair into the `Lsm`, returning the
    /// previous value if it existed. This operation might
    /// involve blocking for a very brief moment as a 32kb
    /// `BufWriter` wrapping the log file is flushed.
    ///
    /// If you require blocking until all written data is
    /// durable, use the `Lsm::flush` method below.
    pub fn insert(&mut self, k: [u8; K], v: [u8; V]) -> Result<Option<[u8; V]>> {
        assert_ne!([k[0], v[0]], [255, 254]);
        self.log_mutation(k, Some(v))?;
        Ok(self.db.insert(k, v))
    }

    /// Removes a KV pair from the `Lsm`, returning the
    /// previous value if it existed. This operation might
    /// involve blocking for a very brief moment as a 32kb
    /// `BufWriter` wrapping the log file is flushed.
    ///
    /// If you require blocking until all written data is
    /// durable, use the `Lsm::flush` method below.
    pub fn remove(&mut self, k: &[u8; K]) -> Result<Option<[u8; V]>> {
        self.log_mutation(*k, None)?;
        Ok(self.db.remove(k))
    }

    fn log_mutation(&mut self, k: [u8; K], v: Option<[u8; V]>) -> Result<()> {
        let crc: u32 = hash(&k, &v);
        self.log.write(&crc.to_le_bytes())?;
        self.log.write(&[v.is_some() as u8])?;
        self.log.write(&k)?;

        if let Some(v) = v {
            self.log.write(&v)?;
        } else {
            self.log.write(&[0; V])?;
        };

        self.memtable.insert(k, v);

        self.dirty_bytes += 4 + K + V;

        if self.dirty_bytes > self.config.max_log_length {
            self.flush()?;
        }

        Ok(())
    }

    /// Blocks until all log data has been
    /// written out to disk and fsynced. If
    /// the log file has grown above a certain
    /// threshold, it will be compacted into
    /// a new sstable and the log file will
    /// be truncated after the sstable has
    /// been written, fsynced, and the sstable
    /// directory has been fsyced.
    pub fn flush(&mut self) -> Result<()> {
        self.log.flush()?;
        self.log.get_mut().sync_all()?;

        if self.dirty_bytes > self.config.max_log_length {
            log::debug!("flushing log");
            let memtable = std::mem::take(&mut self.memtable);
            let sst_id = self.next_sstable_id;
            if let Err(e) = write_sstable(&self.path, sst_id, &memtable, false, &self.config) {
                // put memtable back together before returning
                self.memtable = memtable;
                log::error!("failed to flush lsm log to sst: {:?}", e);
                return Err(e.into());
            }

            let sst_sz = 8 + (memtable.len() as u64 * (4 + K + V) as u64);
            let db_sz = self.db.len() as u64 * (K + V) as u64;

            self.worker_outbox
                .send(WorkerMessage::NewSST {
                    id: sst_id,
                    sst_sz,
                    db_sz,
                })
                .expect("compactor must outlive db");

            self.next_sstable_id += 1;

            assert_eq!(self.log.buffer().len(), 0);

            let log_file: &mut fs::File = self.log.get_mut();
            log_file.seek(io::SeekFrom::Start(0))?;
            log_file.set_len(0)?;
            log_file.sync_all()?;
            fs::File::open(self.path.join(SSTABLE_DIR))?.sync_all()?;

            self.dirty_bytes = 0;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests;
