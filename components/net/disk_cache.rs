/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The HTTP disk cache: an SQLite file that keeps complete responses across restarts.
//!
//! Nothing here blocks a fetch on disk I/O it doesn't need:
//! - Which keys are on disk, their sizes and their age live in memory (`State`), loaded once
//!   when the cache opens from a small metadata table that holds no bodies. A miss is answered
//!   from that index without touching SQLite.
//! - Writes (and evictions to keep the cache under its size limit) happen on one writer
//!   thread with its own connection. Entries waiting to be written are served from memory.
//! - Reads run on the blocking thread pool with their own connections; with WAL they never
//!   wait behind a write.
//! - Reading an entry leaves it on disk, and an entry is only written again when it changed
//!   (see `entry_fingerprint`).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use log::{error, info};
use malloc_size_of::{MallocSizeOf, MallocSizeOfOps};
use parking_lot::{Condvar, Mutex};
use rusqlite::{Connection, OptionalExtension, params};
use servo_config::pref;
use tokio::sync::RwLock as TokioRwLock;

use crate::http_cache::{
    CacheEntry, CacheKey, CachedResource, HttpCacheAssignment, MemoryCacheLifecycle,
    entry_body_bytes, entry_fingerprint,
};

/// Bodies, one row per URL. This is the table earlier versions used (they also read their
/// metadata from it, which meant reading every body); its layout is unchanged so that their
/// cache files keep working.
const CREATE_DATA_TABLE: &str = "CREATE TABLE IF NOT EXISTS disk_cache (
    key TEXT NOT NULL PRIMARY KEY,
    data BLOB NOT NULL,
    size INTEGER NOT NULL,
    insertion_timestamp INTEGER NOT NULL)";

/// Everything but the bodies: read once at startup to build the in-memory index.
/// `seq` orders entries from oldest to newest; `fingerprint` identifies the stored version
/// (0: unknown, for entries carried over from an older cache file).
const CREATE_META_TABLE: &str = "CREATE TABLE IF NOT EXISTS disk_cache_meta (
    key TEXT NOT NULL PRIMARY KEY,
    size INTEGER NOT NULL,
    seq INTEGER NOT NULL,
    fingerprint INTEGER NOT NULL) WITHOUT ROWID";

/// Write at most this many entries, or about this many bytes, per transaction, so newly queued
/// small entries don't wait long behind big ones.
const ENTRIES_PER_TRANSACTION: usize = 16;
const BYTES_PER_TRANSACTION: usize = 2 * 1024 * 1024;

/// What a round of writing left to do.
#[derive(Default)]
struct WriteProgress {
    /// More entries are queued.
    more: bool,
    /// Some entries are locked by a fetch: retry soon.
    busy: bool,
}
/// Truncate the WAL once it is fully copied into the file and bigger than this many pages
/// (4 KB each).
const TRUNCATE_WAL_PAGES: i64 = 1024;
/// Read connections (reads beyond this many at once wait for one; a read takes well under a
/// millisecond once the page is cached by the OS).
const READERS: usize = 4;

struct IndexEntry {
    size: u64,
    seq: u64,
    fingerprint: u64,
}

#[derive(Default)]
struct State {
    /// The entries on disk.
    index: HashMap<String, IndexEntry>,
    /// The keys of `index`, oldest (least recently stored or read) first.
    order: BTreeMap<u64, String>,
    /// The sum of the sizes in `index`.
    total: u64,
    next_seq: u64,
    /// Entries waiting for the writer, with the sequence number they were queued at (their
    /// age once written).
    pending: HashMap<String, (CacheEntry, u64)>,
}

impl State {
    fn take_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        seq
    }

    fn insert(&mut self, key: String, size: u64, seq: u64, fingerprint: u64) {
        self.remove(&key);
        self.next_seq = self.next_seq.max(seq + 1);
        self.order.insert(seq, key.clone());
        self.total += size;
        self.index.insert(
            key,
            IndexEntry {
                size,
                seq,
                fingerprint,
            },
        );
    }

    fn remove(&mut self, key: &str) {
        if let Some(entry) = self.index.remove(key) {
            self.order.remove(&entry.seq);
            self.total -= entry.size;
        }
    }

    /// Mark `key` as the most recently used entry.
    fn touch(&mut self, key: &str) {
        let seq = self.take_seq();
        if let Some(entry) = self.index.get_mut(key) {
            self.order.remove(&entry.seq);
            entry.seq = seq;
            self.order.insert(seq, key.to_owned());
        }
    }

    fn clear_index(&mut self) {
        self.index.clear();
        self.order.clear();
        self.total = 0;
    }

    /// Remove the oldest entries from the index until the total fits `max_size`; returns
    /// their keys so their rows can be deleted.
    fn evict_to(&mut self, max_size: u64) -> Vec<String> {
        let mut evicted = vec![];
        while self.total > max_size {
            let Some((_, key)) = self.order.pop_first() else {
                error!("Disk cache size accounting is off by {} bytes", self.total);
                self.total = 0;
                break;
            };
            if let Some(entry) = self.index.remove(&key) {
                self.total -= entry.size;
            }
            evicted.push(key);
        }
        evicted
    }
}

/// What is shared between the cache, its writer thread and its readers.
struct Shared {
    path: PathBuf,
    max_size: u64,
    state: Mutex<State>,
    readers: Mutex<Readers>,
    /// Signalled when a read connection is returned to `readers`.
    reader_returned: Condvar,
}

/// Read connections, opened once and reused: opening one reads the file's header and
/// schema, which takes tens of milliseconds on a slow disk.
#[derive(Default)]
struct Readers {
    idle: Vec<Connection>,
    /// How many are open (idle or in use), at most `READERS`.
    open: usize,
}

enum Command {
    /// Entries were added to `State::pending`.
    Store,
    /// Delete everything.
    Clear,
    /// Delete one entry (already gone from the index).
    Delete(String),
}

/// The disk cache.
pub(crate) struct DiskCache {
    shared: Arc<Shared>,
    commands: Mutex<Sender<Command>>,
}

impl MallocSizeOf for DiskCache {
    fn size_of(&self, _ops: &mut MallocSizeOfOps) -> usize {
        // The index: a key and a few numbers per entry, in a hash map and a tree.
        let state = self.shared.state.lock();
        state
            .index
            .keys()
            .map(|key| key.capacity() * 2 + 96)
            .sum()
    }
}

/// Get the storage path out of `network_http_disk_cache` preference and `temporary_storage` option.
fn storage_dir() -> Option<PathBuf> {
    let disk_storage_path = pref!(network_http_disk_cache);
    match (
        servo_config::opts::get().temporary_storage,
        disk_storage_path.is_empty(),
    ) {
        (false, false) => Some(disk_storage_path.into()),
        (true, true) => {
            let tmp_dir = tempfile::tempdir().unwrap();
            let mut path = tmp_dir.path().to_path_buf();
            path.set_file_name("cache.sqlite3");
            Some(path)
        },
        (true, false) => {
            error!(
                "Temporary storage cannot be set with explicit disk storage path. Disabling http_disk_cache"
            );
            None
        },
        (false, true) => None,
    }
}

fn open_reader(path: &Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    Ok(connection)
}

fn open_writer(path: &Path) -> rusqlite::Result<Connection> {
    let connection = open_reader(path)?;
    connection.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
    // With WAL, NORMAL only risks losing the last writes on power loss, never corruption.
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    connection.query_row("PRAGMA journal_size_limit = 16777216", [], |_| Ok(()))?;
    // Checkpoints (copying the WAL into the file, with fsyncs) take seconds on a slow disk:
    // they run on their own thread (`run_checkpointer`) so writes don't wait for them.
    connection.query_row("PRAGMA wal_autocheckpoint = 0", [], |_| Ok(()))?;
    connection.execute_batch(CREATE_DATA_TABLE)?;
    connection.execute_batch(CREATE_META_TABLE)?;
    Ok(connection)
}

/// Copy written pages from the WAL into the database file whenever the writer has written
/// something, without blocking it or the readers.
fn run_checkpointer(path: PathBuf, written: Receiver<()>) {
    let Ok(connection) = open_reader(&path) else {
        return;
    };
    while written.recv().is_ok() {
        while written.try_recv().is_ok() {}
        let start = Instant::now();
        let result = connection.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
            Ok((row.get::<_, i64>(1)?, row.get::<_, i64>(2)?))
        });
        match result {
            Ok((frames, done)) => {
                // Everything is in the file: give a big WAL's space back (this only waits
                // for a write or read in progress).
                if frames == done && frames > TRUNCATE_WAL_PAGES {
                    let _ = connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
                }
                info!(
                    target: "cache_timing",
                    "disk checkpoint: {done} of {frames} WAL pages, {} us",
                    start.elapsed().as_micros()
                )
            },
            Err(error) => error!("Disk cache checkpoint failed: {error}"),
        }
    }
}

/// Make the metadata table describe exactly the rows of the data table. A cache file from an
/// older version has no metadata yet: build it from the data table without reading bodies
/// (`length()` of a blob comes from the row header, and `key` precedes `data`).
fn sync_metadata(connection: &mut Connection) -> rusqlite::Result<()> {
    let missing_meta: i64 = connection.query_row(
        "SELECT count(*) FROM disk_cache AS d WHERE NOT EXISTS
            (SELECT 1 FROM disk_cache_meta AS m WHERE m.key = d.key)",
        [],
        |row| row.get(0),
    )?;
    let missing_data: i64 = connection.query_row(
        "SELECT count(*) FROM disk_cache_meta AS m WHERE NOT EXISTS
            (SELECT 1 FROM disk_cache AS d WHERE d.key = m.key)",
        [],
        |row| row.get(0),
    )?;
    if missing_meta == 0 && missing_data == 0 {
        return Ok(());
    }
    let transaction = connection.transaction()?;
    transaction.execute(
        "DELETE FROM disk_cache_meta WHERE NOT EXISTS
            (SELECT 1 FROM disk_cache AS d WHERE d.key = disk_cache_meta.key)",
        [],
    )?;
    transaction.execute(
        "INSERT INTO disk_cache_meta (key, size, seq, fingerprint)
            SELECT key, length(data), rowid, 0 FROM disk_cache AS d WHERE NOT EXISTS
                (SELECT 1 FROM disk_cache_meta AS m WHERE m.key = d.key)",
        [],
    )?;
    transaction.commit()
}

fn load_index(connection: &Connection) -> rusqlite::Result<State> {
    let mut state = State::default();
    let mut statement =
        connection.prepare("SELECT key, size, seq, fingerprint FROM disk_cache_meta")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })?;
    for row in rows {
        let (key, size, seq, fingerprint) = row?;
        state.insert(key, size.max(0) as u64, seq.max(0) as u64, fingerprint as u64);
    }
    Ok(state)
}

impl DiskCache {
    /// Opens the disk cache if the preference is set: creates or upgrades its tables, loads
    /// the index and starts the writer thread.
    pub(crate) fn new(
        cache_assignment: HttpCacheAssignment,
    ) -> (Option<Arc<DiskCache>>, MemoryCacheLifecycle) {
        // For private browsing we currently do not want to store any disk cache.
        if cache_assignment != HttpCacheAssignment::Public {
            return (None, MemoryCacheLifecycle::empty());
        }
        let Some(path) = storage_dir() else {
            return (None, MemoryCacheLifecycle::empty());
        };
        let max_size: u64 = pref!(network_http_disk_cache_size);
        let start = Instant::now();
        let opened = open_writer(&path).and_then(|mut connection| {
            sync_metadata(&mut connection)?;
            let state = load_index(&connection)?;
            Ok((connection, state))
        });
        let (connection, state) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                error!("Could not open the disk cache: {error}");
                return (None, MemoryCacheLifecycle::empty());
            },
        };
        info!(
            target: "cache_timing",
            "disk cache opened: {} entries, {} bytes, {} us",
            state.index.len(),
            state.total,
            start.elapsed().as_micros()
        );

        // One ready for the first lookup; the writer thread opens the others.
        let idle: Vec<Connection> = open_reader(&path).into_iter().collect();
        let readers = Readers {
            open: idle.len(),
            idle,
        };
        let shared = Arc::new(Shared {
            path,
            max_size,
            state: Mutex::new(state),
            readers: Mutex::new(readers),
            reader_returned: Condvar::new(),
        });
        let (sender, receiver) = channel();
        let writer_shared = shared.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("DiskCacheWriter".into())
            .spawn(move || writer_shared.run_writer(connection, receiver))
        {
            error!("Could not start the disk cache writer: {error}");
            return (None, MemoryCacheLifecycle::empty());
        }

        let disk_cache = Arc::new(DiskCache {
            shared,
            commands: Mutex::new(sender),
        });
        (
            Some(disk_cache.clone()),
            MemoryCacheLifecycle {
                disk_cache: Some(disk_cache),
            },
        )
    }

    /// Returns the entry for `key` if the disk cache has it. The entry stays on disk.
    #[servo_tracing::instrument(skip(self))]
    pub(crate) async fn get(&self, key: CacheKey) -> Option<CacheEntry> {
        let key = key.as_ref().to_owned();
        {
            let state = self.shared.state.lock();
            if let Some((entry, _)) = state.pending.get(&key) {
                return Some(entry.clone());
            }
            if !state.index.contains_key(&key) {
                return None;
            }
        }
        let shared = self.shared.clone();
        tokio::task::spawn_blocking(move || shared.read(&key))
            .await
            .ok()
            .flatten()
    }

    /// Queue `entry` to be written to disk, unless the disk already has this version of it.
    pub(crate) fn store(&self, key: &CacheKey, entry: CacheEntry) {
        {
            let mut state = self.shared.state.lock();
            let generation = state.take_seq();
            state
                .pending
                .insert(key.as_ref().to_owned(), (entry, generation));
        }
        let _ = self.commands.lock().send(Command::Store);
    }

    /// Removes the entry for `key`, if any.
    pub(crate) fn remove(&self, key: &CacheKey) {
        let key = key.as_ref();
        {
            let mut state = self.shared.state.lock();
            if !state.index.contains_key(key) && !state.pending.contains_key(key) {
                return;
            }
            state.pending.remove(key);
            state.remove(key);
        }
        let _ = self.commands.lock().send(Command::Delete(key.to_owned()));
    }

    /// Clears the disk cache.
    #[servo_tracing::instrument(skip(self))]
    pub(crate) fn clear(&self) {
        {
            let mut state = self.shared.state.lock();
            state.clear_index();
            state.pending.clear();
        }
        let _ = self.commands.lock().send(Command::Clear);
    }
}

impl Shared {
    /// An idle read connection; opens one if fewer than `READERS` are open, else waits.
    fn take_reader(&self) -> Option<Connection> {
        {
            let mut readers = self.readers.lock();
            loop {
                if let Some(connection) = readers.idle.pop() {
                    return Some(connection);
                }
                if readers.open < READERS {
                    readers.open += 1;
                    break;
                }
                self.reader_returned.wait(&mut readers);
            }
        }
        match open_reader(&self.path) {
            Ok(connection) => Some(connection),
            Err(error) => {
                error!("Could not open the disk cache for reading: {error}");
                self.readers.lock().open -= 1;
                self.reader_returned.notify_one();
                None
            },
        }
    }

    fn return_reader(&self, connection: Connection) {
        self.readers.lock().idle.push(connection);
        self.reader_returned.notify_one();
    }

    /// Open the remaining read connections ahead of the first burst of lookups.
    fn open_readers(&self) {
        loop {
            {
                let mut readers = self.readers.lock();
                if readers.open >= READERS {
                    return;
                }
                readers.open += 1;
            }
            match open_reader(&self.path) {
                Ok(connection) => self.return_reader(connection),
                Err(_) => {
                    self.readers.lock().open -= 1;
                    return;
                },
            }
        }
    }

    fn read(&self, key: &str) -> Option<CacheEntry> {
        let _span = profile_traits::trace_span!("query disk cache").entered();
        let start = Instant::now();
        let connection = self.take_reader()?;
        let bytes = connection
            .prepare_cached("SELECT data FROM disk_cache WHERE key = ?1")
            .and_then(|mut statement| {
                statement
                    .query_row([key], |row| row.get::<_, Vec<u8>>(0))
                    .optional()
            });
        self.return_reader(connection);
        let bytes = match bytes {
            Ok(bytes) => bytes?,
            Err(error) => {
                error!("Could not read from the disk cache: {error}");
                return None;
            },
        };

        let _span = profile_traits::trace_span!("deserialize cache request").entered();
        let Ok(resources) = postcard::from_bytes::<Vec<CachedResource>>(&bytes) else {
            error!("Could not deserialize cached resource");
            return None;
        };
        drop(bytes);
        {
            let mut state = self.state.lock();
            state.touch(key);
            // Entries from an older cache file have no fingerprint yet: this is the version
            // on disk, so storing it unchanged later writes nothing.
            if let Some(entry) = state.index.get_mut(key) &&
                entry.fingerprint == 0
            {
                entry.fingerprint = entry_fingerprint(&resources);
            }
        }
        info!(
            target: "cache_timing",
            "disk read {}us {key}",
            start.elapsed().as_micros()
        );
        Some(std::sync::Arc::new(TokioRwLock::new(resources)))
    }

    fn run_writer(&self, mut connection: Connection, commands: Receiver<Command>) {
        self.open_readers();
        let (checkpoint, written) = channel();
        let path = self.path.clone();
        let _ = std::thread::Builder::new()
            .name("DiskCacheCheckpoint".into())
            .spawn(move || run_checkpointer(path, written));
        self.trim(&mut connection);
        // Also takes in what an earlier run left in the WAL.
        let _ = checkpoint.send(());
        let mut progress = WriteProgress::default();
        loop {
            let mut received = vec![];
            let next = if progress.more {
                commands.try_recv().map_err(|error| match error {
                    TryRecvError::Empty => RecvTimeoutError::Timeout,
                    TryRecvError::Disconnected => RecvTimeoutError::Disconnected,
                })
            } else if progress.busy {
                commands.recv_timeout(Duration::from_millis(50))
            } else {
                commands.recv().map_err(|_| RecvTimeoutError::Disconnected)
            };
            match next {
                Ok(command) => received.push(command),
                Err(RecvTimeoutError::Timeout) => {},
                Err(RecvTimeoutError::Disconnected) => {
                    while self.write_some(&mut connection).more {}
                    self.trim(&mut connection);
                    return;
                },
            }
            received.extend(commands.try_iter());
            // Deletions were requested before the entries now queued were stored, so they go
            // first.
            for command in received {
                match command {
                    Command::Store => {},
                    Command::Clear => self.clear_disk(&connection),
                    Command::Delete(key) => {
                        let result = connection.transaction().and_then(|transaction| {
                            delete_rows(&transaction, std::slice::from_ref(&key))?;
                            transaction.commit()
                        });
                        if let Err(error) = result {
                            error!("Could not delete a disk cache entry: {error}");
                        }
                        // A write that was under way when it was removed may have put it back.
                        let mut state = self.state.lock();
                        if !state.pending.contains_key(&key) {
                            state.remove(&key);
                        }
                    },
                }
            }
            progress = self.write_some(&mut connection);
            let _ = checkpoint.send(());
        }
    }

    fn clear_disk(&self, connection: &Connection) {
        if let Err(error) =
            connection.execute_batch("DELETE FROM disk_cache; DELETE FROM disk_cache_meta;")
        {
            error!("Could not clear the disk cache: {error}");
        }
        self.state.lock().clear_index();
    }

    /// Write one transaction's worth of the queued entries, smallest first: on a slow disk
    /// a page's scripts and stylesheets shouldn't wait behind big downloads (entries still
    /// queued when the browser exits are lost).
    fn write_some(&self, connection: &mut Connection) -> WriteProgress {
        let start = Instant::now();
        let queued: Vec<(String, CacheEntry, u64)> = self
            .state
            .lock()
            .pending
            .iter()
            .map(|(key, (entry, generation))| (key.clone(), entry.clone(), *generation))
            .collect();
        if queued.is_empty() {
            return WriteProgress::default();
        }
        info!(target: "cache_timing", "disk writer: {} queued", queued.len());
        let mut busy = false;
        let mut sized: Vec<(usize, (String, CacheEntry, u64))> = queued
            .into_iter()
            .filter_map(|item| {
                let bytes = item.1.try_read().map(|resources| entry_body_bytes(&resources));
                match bytes {
                    Ok(bytes) => Some((bytes, item)),
                    Err(_) => {
                        busy = true;
                        None
                    },
                }
            })
            .collect();
        sized.sort_by_key(|(bytes, (_, _, generation))| (*bytes, *generation));
        let mut bytes = 0;
        let mut chunk = vec![];
        let mut rest = sized.into_iter();
        for (size, item) in rest.by_ref() {
            bytes += size;
            chunk.push(item);
            if chunk.len() >= ENTRIES_PER_TRANSACTION || bytes >= BYTES_PER_TRANSACTION {
                break;
            }
        }
        let more = rest.next().is_some();
        let (written, chunk_busy) = self.write_chunk(connection, &chunk);
        if written > 0 {
            info!(
                target: "cache_timing",
                "disk write: {written} of {} entries, {bytes} bytes, {} us, total {} bytes",
                chunk.len(),
                start.elapsed().as_micros(),
                self.state.lock().total
            );
        }
        WriteProgress {
            more,
            busy: busy || chunk_busy,
        }
    }

    /// Returns how many entries were written, and whether some were busy.
    fn write_chunk(
        &self,
        connection: &mut Connection,
        chunk: &[(String, CacheEntry, u64)],
    ) -> (usize, bool) {
        enum Outcome {
            /// Nothing to write: the disk has this version, or nothing in it is complete.
            Unchanged,
            Stored { size: u64, seq: u64, fingerprint: u64 },
            Removed,
            /// Locked by a fetch; try again later.
            Busy,
        }

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0) as i64;
        let transaction = match connection.transaction() {
            Ok(transaction) => transaction,
            Err(error) => {
                error!("Could not write to the disk cache: {error}");
                self.forget_pending(chunk);
                return (0, false);
            },
        };
        let mut outcomes = Vec::with_capacity(chunk.len());
        for (key, entry, generation) in chunk {
            let Ok(resources) = entry.try_read() else {
                outcomes.push(Outcome::Busy);
                continue;
            };
            let fingerprint = entry_fingerprint(&resources);
            let complete: Vec<&CachedResource> = resources
                .iter()
                .filter(|resource| resource.is_done())
                .collect();
            let unchanged = self
                .state
                .lock()
                .index
                .get(key)
                .is_some_and(|stored| stored.fingerprint == fingerprint);
            if complete.is_empty() || unchanged {
                outcomes.push(Outcome::Unchanged);
                continue;
            }
            // Size the buffer exactly: growing it while serializing briefly needs up to twice
            // the body on top of the body itself.
            let data = postcard::serialize_with_flavor(
                &*complete,
                postcard::ser_flavors::Size::default(),
            )
            .and_then(|size| postcard::to_extend(&*complete, Vec::with_capacity(size)));
            drop(complete);
            drop(resources);
            let Ok(data) = data else {
                error!("Could not serialize a cache entry");
                outcomes.push(Outcome::Unchanged);
                continue;
            };
            let size = data.len() as u64;
            let result = if size > self.max_size {
                transaction
                    .execute("DELETE FROM disk_cache WHERE key = ?1", [key])
                    .and_then(|_| {
                        transaction.execute("DELETE FROM disk_cache_meta WHERE key = ?1", [key])
                    })
                    .map(|_| Outcome::Removed)
            } else {
                // Entries age from when they were last stored or used, not from when the
                // writer got to them.
                let seq = *generation;
                transaction
                    .prepare_cached(
                        "INSERT INTO disk_cache (key, data, size, insertion_timestamp)
                            VALUES (?1, ?2, ?3, ?4)
                            ON CONFLICT(key) DO UPDATE SET data = excluded.data,
                                size = excluded.size,
                                insertion_timestamp = excluded.insertion_timestamp",
                    )
                    .and_then(|mut statement| {
                        statement.execute(params![key, data, size as i64, timestamp])
                    })
                    .and_then(|_| {
                        transaction
                            .prepare_cached(
                                "INSERT INTO disk_cache_meta (key, size, seq, fingerprint)
                                    VALUES (?1, ?2, ?3, ?4)
                                    ON CONFLICT(key) DO UPDATE SET size = excluded.size,
                                        seq = excluded.seq, fingerprint = excluded.fingerprint",
                            )?
                            .execute(params![key, size as i64, seq as i64, fingerprint as i64])
                    })
                    .map(|_| Outcome::Stored {
                        size,
                        seq,
                        fingerprint,
                    })
            };
            outcomes.push(result.unwrap_or_else(|error| {
                error!("Could not write a cache entry: {error}");
                Outcome::Unchanged
            }));
        }
        // Update the index and delete the oldest entries in the same transaction, so the
        // file never holds more than the size limit. (Until the commit, lookups of these
        // entries are answered from `pending`.)
        let mut written = 0;
        let mut busy = false;
        let evicted = {
            let mut state = self.state.lock();
            for ((key, _, _), outcome) in chunk.iter().zip(&outcomes) {
                match *outcome {
                    Outcome::Stored {
                        size,
                        seq,
                        fingerprint,
                    } => {
                        written += 1;
                        state.insert(key.clone(), size, seq, fingerprint)
                    },
                    Outcome::Removed => state.remove(key),
                    Outcome::Busy => busy = true,
                    Outcome::Unchanged => {},
                }
            }
            state.evict_to(self.max_size)
        };
        let deleted = delete_rows(&transaction, &evicted);
        let commit_start = Instant::now();
        if let Err(error) = deleted.and_then(|_| transaction.commit()) {
            error!("Could not write to the disk cache: {error}");
            self.forget_pending(chunk);
            return (0, false);
        }
        info!(
            target: "cache_timing",
            "disk commit: {} entries, {} evicted, {} us",
            chunk.len(),
            evicted.len(),
            commit_start.elapsed().as_micros()
        );

        let mut state = self.state.lock();
        for ((key, _, generation), outcome) in chunk.iter().zip(outcomes) {
            // An entry queued again meanwhile (it may have changed) stays queued.
            if !matches!(outcome, Outcome::Busy) &&
                state
                    .pending
                    .get(key)
                    .is_some_and(|(_, queued)| queued == generation)
            {
                state.pending.remove(key);
            }
        }
        (written, busy)
    }

    fn forget_pending(&self, chunk: &[(String, CacheEntry, u64)]) {
        let mut state = self.state.lock();
        for (key, _, generation) in chunk {
            if state
                .pending
                .get(key)
                .is_some_and(|(_, queued)| queued == generation)
            {
                state.pending.remove(key);
            }
        }
    }

    /// Delete the oldest entries until the cache fits its size limit (when the limit was
    /// lowered, or an older version left the cache over it).
    fn trim(&self, connection: &mut Connection) {
        let evicted = self.state.lock().evict_to(self.max_size);
        if evicted.is_empty() {
            return;
        }
        let result = connection.transaction().and_then(|transaction| {
            delete_rows(&transaction, &evicted)?;
            transaction.commit()
        });
        if let Err(error) = result {
            error!("Could not delete old disk cache entries: {error}");
        }
        info!(target: "cache_timing", "disk trim: {} entries deleted", evicted.len());
    }
}

fn delete_rows(connection: &Connection, keys: &[String]) -> rusqlite::Result<()> {
    for key in keys {
        connection
            .prepare_cached("DELETE FROM disk_cache WHERE key = ?1")?
            .execute([key])?;
        connection
            .prepare_cached("DELETE FROM disk_cache_meta WHERE key = ?1")?
            .execute([key])?;
    }
    Ok(())
}
