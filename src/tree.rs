use crate::{
    block_cache::BlockCache,
    compaction::{worker::start_compaction_thread, CompactionStrategy},
    descriptor_table::FileDescriptorTable,
    entry::{OccupiedEntry, VacantEntry},
    file::{BLOCKS_FILE, JOURNALS_FOLDER, LEVELS_MANIFEST_FILE, LSM_MARKER, SEGMENTS_FOLDER},
    id::generate_segment_id,
    journal::{shard::JournalShard, Journal},
    levels::Levels,
    memtable::MemTable,
    prefix::Prefix,
    range::{MemTableGuard, Range},
    segment::{self, meta::Metadata, Segment},
    stop_signal::StopSignal,
    tree_inner::TreeInner,
    value::{SeqNo, UserData, UserKey, ValueType},
    Batch, Config, Snapshot, Value,
};
use std::{
    collections::HashMap,
    ops::RangeBounds,
    path::Path,
    sync::{
        atomic::{AtomicU32, AtomicU64},
        Arc, RwLock, RwLockWriteGuard,
    },
};
use std_semaphore::Semaphore;

pub struct CompareAndSwapError {
    /// The value currently in the tree that caused the CAS error
    pub prev: Option<UserData>,

    /// The value that was proposed
    pub next: Option<UserData>,
}

pub type CompareAndSwapResult = Result<(), CompareAndSwapError>;

/// A log-structured merge tree (LSM-tree/LSMT)
///
/// The tree is internally synchronized (Send + Sync), so it does not need to be wrapped in a lock nor an Arc.
///
/// To share the tree between threads, use `Arc::clone(&tree)` or `tree.clone()`.
#[doc(alias = "keyspace")]
#[doc(alias = "table")]
#[derive(Clone)]
pub struct Tree(Arc<TreeInner>);

impl std::ops::Deref for Tree {
    type Target = Arc<TreeInner>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

fn ignore_tombstone_value(item: Value) -> Option<Value> {
    if item.is_tombstone() {
        None
    } else {
        Some(item)
    }
}

impl Tree {
    /// Opens the tree at the given folder.
    ///
    /// Will create a new tree if the folder is not in use
    /// or recover a previous state if it exists.
    ///
    /// # Examples
    ///
    /// ```
    /// # let folder = tempfile::tempdir()?;
    /// use lsm_tree::{Config, Tree};
    ///
    /// let tree = Tree::open(Config::new(folder))?;
    /// // Same as
    /// # let folder = tempfile::tempdir()?;
    /// let tree = Config::new(folder).open()?;
    /// #
    /// # Ok::<(), lsm_tree::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn open(config: Config) -> crate::Result<Self> {
        log::info!("Opening LSM-tree at {}", config.path.display());

        let flush_ms = config.fsync_ms;

        let tree = if config.path.join(LSM_MARKER).exists() {
            Self::recover(config)
        } else {
            Self::create_new(config)
        };

        if let Some(ms) = flush_ms {
            if let Ok(tree) = &tree {
                tree.start_fsync_thread(ms);
            };
        }

        tree
    }

    fn start_fsync_thread(&self, ms: usize) {
        log::debug!("starting fsync thread");

        let journal = Arc::clone(&self.journal);
        let stop_signal = self.stop_signal.clone();

        std::thread::spawn(move || loop {
            log::trace!("fsync thread: sleeping {ms}ms");
            std::thread::sleep(std::time::Duration::from_millis(ms as u64));

            if stop_signal.is_stopped() {
                log::debug!("fsync thread: exiting because tree is dropping");
                return;
            }

            log::trace!("fsync thread: fsycing journal");
            if let Err(e) = journal.flush() {
                log::error!("Fsync failed: {e:?}");
            }
        });
    }

    /// Gets the given key’s corresponding entry in the map for in-place manipulation.
    ///
    /// # Examples
    ///
    /// ```
    /// # let folder = tempfile::tempdir()?;
    /// use lsm_tree::{Config, Tree};
    ///
    /// let tree = Tree::open(Config::new(folder))?;
    ///
    /// let value = tree.entry("a")?.or_insert("abc")?;
    /// assert_eq!("abc".as_bytes(), &*value);
    /// #
    /// # Ok::<(), lsm_tree::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn entry<K: AsRef<[u8]>>(&self, key: K) -> crate::Result<crate::entry::Entry> {
        let key = key.as_ref();
        let item = self.get_internal_entry(key, true, None)?;

        Ok(match item {
            Some(item) => crate::entry::Entry::Occupied(OccupiedEntry {
                tree: self.clone(),
                key: key.to_vec().into(),
                value: item.value,
            }),
            None => crate::entry::Entry::Vacant(VacantEntry {
                tree: self.clone(),
                key: key.to_vec().into(),
            }),
        })
    }

    /// Opens a read-only point-in-time snapshot of the tree
    ///
    /// Dropping the snapshot will close the snapshot
    ///
    /// # Examples
    ///
    /// ```
    /// # let folder = tempfile::tempdir()?;
    /// use lsm_tree::{Config, Tree};
    ///
    /// let tree = Config::new(folder).open()?;
    ///
    /// tree.insert("a", "abc")?;
    ///
    /// let snapshot = tree.snapshot();
    /// assert_eq!(snapshot.len()?, tree.len()?);
    ///
    /// tree.insert("b", "abc")?;
    ///
    /// assert_eq!(2, tree.len()?);
    /// assert_eq!(1, snapshot.len()?);
    ///
    /// assert!(snapshot.contains_key("a")?);
    /// assert!(!snapshot.contains_key("b")?);
    /// #
    /// # Ok::<(), lsm_tree::Error>(())
    /// ```
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        Snapshot::new(self.clone())
    }

    /// Initializes a new, atomic write batch.
    ///
    /// Call [`Batch::commit`] to commit the batch to the tree.
    ///
    /// Dropping the batch will not commit items to the tree.
    ///
    /// # Examples
    ///
    /// ```
    /// # let folder = tempfile::tempdir()?;
    /// use lsm_tree::{Config, Tree};
    ///
    /// let tree = Config::new(folder).open()?;
    ///
    /// let mut batch = tree.batch();
    /// batch.insert("a", "hello");
    /// batch.insert("b", "hello2");
    /// batch.insert("c", "hello3");
    /// batch.remove("idontlikeu");
    ///
    /// batch.commit()?;
    ///
    /// assert_eq!(3, tree.len()?);
    /// #
    /// # Ok::<(), lsm_tree::Error>(())
    /// ```
    #[must_use]
    pub fn batch(&self) -> Batch {
        Batch::new(self.clone())
    }

    /// Returns `true` if there are some segments that are being compacted.
    #[doc(hidden)]
    #[must_use]
    pub fn is_compacting(&self) -> bool {
        let levels = self.levels.read().expect("lock is poisoned");
        levels.is_compacting()
    }

    /// Counts the amount of segments currently in the tree.
    #[doc(hidden)]
    #[must_use]
    pub fn segment_count(&self) -> usize {
        self.levels.read().expect("lock is poisoned").len()
    }

    /// Sums the disk space usage of the tree (segments + journals).
    ///
    /// # Examples
    ///
    /// ```
    /// # let folder = tempfile::tempdir()?;
    /// use lsm_tree::{Config, Tree};
    ///
    /// let tree = Config::new(folder).open()?;
    /// assert_eq!(0, tree.disk_space()?);
    /// #
    /// # Ok::<(), lsm_tree::Error>(())
    /// ```
    pub fn disk_space(&self) -> crate::Result<u64> {
        let segment_size = self
            .levels
            .read()
            .expect("lock is poisoned")
            .get_all_segments()
            .values()
            .map(|x| x.metadata.file_size)
            .sum::<u64>();

        // TODO: replace fs extra with Journal::disk_space
        let active_journal_size = fs_extra::dir::get_size(&self.journal.path)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "fs_extra error"))?;

        Ok(segment_size + active_journal_size)
    }

    /// Returns the tree configuration.
    ///
    /// # Examples
    ///
    /// ```
    /// # let folder = tempfile::tempdir()?;
    /// use lsm_tree::{Config, Tree};
    ///
    /// let tree = Config::new(folder).open()?;
    ///
    /// assert_eq!(Config::default().block_size, tree.config().block_size);
    /// #
    /// # Ok::<(), lsm_tree::Error>(())
    /// ```
    #[must_use]
    pub fn config(&self) -> Config {
        self.config.clone()
    }

    /// Returns the amount of cached blocks.
    ///
    /// # Examples
    ///
    /// ```
    /// # let folder = tempfile::tempdir()?;
    /// use lsm_tree::{Config, Tree};
    ///
    /// let tree = Config::new(folder).open()?;
    ///
    /// assert_eq!(0, tree.block_cache_size());
    /// #
    /// # Ok::<(), lsm_tree::Error>(())
    /// ```
    #[must_use]
    pub fn block_cache_size(&self) -> usize {
        self.block_cache.len()
    }

    /// Scans the entire tree, returning the amount of items.
    ///
    /// ###### Caution
    ///
    /// This operation scans the entire tree: O(n) complexity!
    ///
    /// Never, under any circumstances, use .len() == 0 to check
    /// if the tree is empty, use [`Tree::is_empty`] instead.
    ///
    /// # Examples
    ///
    /// ```
    /// # use lsm_tree::Error as TreeError;
    /// use lsm_tree::{Tree, Config};
    ///
    /// let folder = tempfile::tempdir()?;
    /// let tree = Config::new(folder).open()?;
    ///
    /// assert_eq!(tree.len()?, 0);
    /// tree.insert("1", "abc")?;
    /// tree.insert("3", "abc")?;
    /// tree.insert("5", "abc")?;
    /// assert_eq!(tree.len()?, 3);
    /// #
    /// # Ok::<(), TreeError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn len(&self) -> crate::Result<usize> {
        Ok(self.iter()?.into_iter().filter(Result::is_ok).count())
    }

    /// Returns `true` if the tree is empty.
    ///
    /// This operation has O(1) complexity.
    ///
    /// # Examples
    ///
    /// ```
    /// # let folder = tempfile::tempdir()?;
    /// use lsm_tree::{Config, Tree};
    ///
    /// let tree = Config::new(folder).open()?;
    /// assert!(tree.is_empty()?);
    ///
    /// tree.insert("a", nanoid::nanoid!())?;
    /// assert!(!tree.is_empty()?);
    /// #
    /// # Ok::<(), lsm_tree::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn is_empty(&self) -> crate::Result<bool> {
        self.first_key_value().map(|x| x.is_none())
    }

    /// Creates a new tree in a folder.
    ///
    /// # Errors
    ///
    /// - Will return `Err` if an IO error occurs
    /// - Will fail, if the folder already occupied
    fn create_new(config: Config) -> crate::Result<Self> {
        log::info!("Creating LSM-tree at {}", config.path.display());

        // Setup folders
        std::fs::create_dir_all(&config.path)?;
        std::fs::create_dir_all(config.path.join(SEGMENTS_FOLDER))?;
        std::fs::create_dir_all(config.path.join(JOURNALS_FOLDER))?;

        let marker = config.path.join(LSM_MARKER);
        assert!(!marker.try_exists()?);

        let first_journal_path = config
            .path
            .join(JOURNALS_FOLDER)
            .join(generate_segment_id());
        let levels = Levels::create_new(config.levels, config.path.join(LEVELS_MANIFEST_FILE))?;

        let block_cache = Arc::new(BlockCache::new(config.block_cache_capacity as usize));

        let compaction_threads = 4; // TODO: config
        let flush_threads = config.flush_threads.into();

        let inner = TreeInner {
            config,
            journal: Arc::new(Journal::create_new(first_journal_path)?),
            active_memtable: Arc::new(RwLock::new(MemTable::default())),
            immutable_memtables: Arc::default(),
            block_cache,
            lsn: AtomicU64::new(0),
            levels: Arc::new(RwLock::new(levels)),
            flush_semaphore: Arc::new(Semaphore::new(flush_threads)),
            compaction_semaphore: Arc::new(Semaphore::new(compaction_threads)), // TODO: config
            approx_active_memtable_size: AtomicU32::default(),
            open_snapshots: Arc::new(AtomicU32::new(0)),
            stop_signal: StopSignal::default(),
        };

        // fsync folder
        let folder = std::fs::File::open(&inner.config.path)?;
        folder.sync_all()?;

        // fsync folder
        let folder = std::fs::File::open(inner.config.path.join(JOURNALS_FOLDER))?;
        folder.sync_all()?;

        // NOTE: Lastly
        // fsync .lsm marker
        // -> the LSM is fully initialized
        let file = std::fs::File::create(marker)?;
        file.sync_all()?;

        Ok(Self(Arc::new(inner)))
    }

    // TODO: move to new module
    fn recover_segments<P: AsRef<Path>>(
        folder: &P,
        block_cache: &Arc<BlockCache>,
    ) -> crate::Result<HashMap<String, Arc<Segment>>> {
        let folder = folder.as_ref();

        // NOTE: First we load the level manifest without any
        // segments just to get the IDs
        // Then we recover the segments and build the actual level manifest
        let levels = Levels::recover(&folder.join(LEVELS_MANIFEST_FILE), HashMap::new())?;
        let segment_ids_to_recover = levels.list_ids();

        let mut segments = HashMap::new();

        for dirent in std::fs::read_dir(folder.join(SEGMENTS_FOLDER))? {
            let dirent = dirent?;
            let path = dirent.path();

            assert!(path.is_dir());

            let segment_id = dirent
                .file_name()
                .to_str()
                .expect("invalid segment folder name")
                .to_owned();
            log::debug!("Recovering segment from {}", path.display());

            if segment_ids_to_recover.contains(&segment_id) {
                let segment = Segment::recover(
                    &path,
                    Arc::clone(block_cache),
                    Arc::new(FileDescriptorTable::new(path.join(BLOCKS_FILE))?),
                )?;
                segments.insert(segment.metadata.id.clone(), Arc::new(segment));
                log::debug!("Recovered segment from {}", path.display());
            } else {
                log::info!("Deleting unfinished segment: {}", path.to_string_lossy());
                std::fs::remove_dir_all(path)?;
            }
        }

        if segments.len() < segment_ids_to_recover.len() {
            log::error!("Expected segments : {segment_ids_to_recover:?}");
            log::error!(
                "Recovered segments: {:?}",
                segments.keys().collect::<Vec<_>>()
            );

            panic!("Some segments were not recovered")
        }

        Ok(segments)
    }

    // TODO: move to new module
    fn recover_active_journal(config: &Config) -> crate::Result<Option<(Journal, MemTable)>> {
        // Load previous levels manifest
        // Add all flushed segments to it, then recover properly
        let mut levels = Levels::recover(&config.path.join(LEVELS_MANIFEST_FILE), HashMap::new())?;

        let mut active_journal = None;

        for dirent in std::fs::read_dir(config.path.join(JOURNALS_FOLDER))? {
            let dirent = dirent?;
            let journal_path = dirent.path();

            assert!(journal_path.is_dir());

            // TODO: replace fs extra with Journal::disk_space
            let journal_size = fs_extra::dir::get_size(&journal_path)
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "fs_extra error"))?;

            if journal_size == 0 {
                std::fs::remove_dir_all(&journal_path)?;
                continue;
            }

            if !journal_path.join(".flush").exists() {
                // TODO: handle this
                assert!(active_journal.is_none(), "Second active journal found :(");

                if journal_size < config.max_memtable_size.into() {
                    log::info!("Setting {} as active journal", journal_path.display());

                    let (recovered_journal, memtable) = Journal::recover(journal_path.clone())?;
                    active_journal = Some((recovered_journal, memtable));

                    continue;
                }

                log::info!(
                    "Flushing active journal because it is too large: {}",
                    dirent.path().to_string_lossy()
                );

                // Journal is too large to be continued to be used
                // Just flush it
            }

            log::info!(
                "Flushing orphaned journal {} to segment",
                dirent.path().to_string_lossy()
            );

            // TODO: optimize this

            let (recovered_journal, memtable) = Journal::recover(journal_path.clone())?;
            log::trace!("Recovered old journal");
            drop(recovered_journal);

            let segment_id = dirent
                .file_name()
                .to_str()
                .expect("invalid journal folder name")
                .to_string();
            let segment_folder = config.path.join(SEGMENTS_FOLDER).join(&segment_id);

            if !levels.contains_id(&segment_id) {
                // The level manifest does not contain the segment
                // If the segment is maybe half written, clean it up here
                // and then write it
                if segment_folder.exists() {
                    std::fs::remove_dir_all(&segment_folder)?;
                }

                let mut segment_writer = segment::writer::Writer::new(segment::writer::Options {
                    path: segment_folder.clone(),
                    evict_tombstones: false,
                    block_size: config.block_size,
                })?;

                for (key, value) in memtable.items {
                    segment_writer.write(Value::from((key, value)))?;
                }

                segment_writer.finish()?;

                if segment_writer.item_count > 0 {
                    let metadata = Metadata::from_writer(segment_id, segment_writer)?;
                    metadata.write_to_file()?;

                    log::info!("Written segment from orphaned journal: {:?}", metadata.id);

                    levels.add_id(metadata.id);
                    levels.write_to_disk()?;
                }
            }

            std::fs::remove_dir_all(journal_path)?;
        }

        Ok(active_journal)
    }

    /// Tries to recover a tree from a folder.
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    fn recover(config: Config) -> crate::Result<Self> {
        log::info!("Recovering tree from {}", config.path.display());

        let start = std::time::Instant::now();

        log::info!("Restoring journal");
        let active_journal = Self::recover_active_journal(&config)?;

        log::info!("Restoring memtable");

        let (journal, memtable) = if let Some(active_journal) = active_journal {
            active_journal
        } else {
            let next_journal_path = config
                .path
                .join(JOURNALS_FOLDER)
                .join(generate_segment_id());
            (Journal::create_new(next_journal_path)?, MemTable::default())
        };

        // TODO: optimize this... do on journal load...
        let lsn = memtable
            .items
            .iter()
            .map(|x| {
                let key = x.key();
                key.seqno + 1
            })
            .max()
            .unwrap_or(0);

        // Load segments
        log::info!("Restoring segments");

        let block_cache = Arc::new(BlockCache::new(config.block_cache_capacity as usize));
        let segments = Self::recover_segments(&config.path, &block_cache)?;

        // Check if a segment has a higher seqno and then take it
        let lsn = lsn.max(
            segments
                .values()
                .map(|x| x.metadata.seqnos.1 + 1)
                .max()
                .unwrap_or(0),
        );

        // Finalize Tree
        log::debug!("Loading level manifest");

        let mut levels = Levels::recover(&config.path.join(LEVELS_MANIFEST_FILE), segments)?;
        levels.sort_levels();

        let compaction_threads = 4; // TODO: config
        let flush_threads = config.flush_threads.into();

        // TODO: replace fs extra with Journal::disk_space
        let active_journal_size = fs_extra::dir::get_size(&journal.path)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "fs_extra error"))?;

        let inner = TreeInner {
            config,
            journal: Arc::new(journal),
            active_memtable: Arc::new(RwLock::new(memtable)),
            immutable_memtables: Arc::default(),
            block_cache,
            lsn: AtomicU64::new(lsn),
            levels: Arc::new(RwLock::new(levels)),
            flush_semaphore: Arc::new(Semaphore::new(flush_threads)),
            compaction_semaphore: Arc::new(Semaphore::new(compaction_threads)),
            approx_active_memtable_size: AtomicU32::new(active_journal_size as u32),
            open_snapshots: Arc::new(AtomicU32::new(0)),
            stop_signal: StopSignal::default(),
        };

        let tree = Self(Arc::new(inner));

        log::debug!("Starting {compaction_threads} compaction threads");
        for _ in 0..compaction_threads {
            start_compaction_thread(&tree);
        }

        log::info!("Tree loaded in {}s", start.elapsed().as_secs_f32());

        Ok(tree)
    }

    fn append_entry(
        &self,
        mut shard: RwLockWriteGuard<'_, JournalShard>,
        value: Value,
    ) -> crate::Result<()> {
        let bytes_written_to_disk = shard.write(&value)?;
        drop(shard);

        let memtable_lock = self.active_memtable.read().expect("lock is poisoned");
        memtable_lock.insert(value);

        // NOTE: Add some pointers to better approximate memory usage of memtable
        // Because the data is stored with less overhead than in memory
        let size = bytes_written_to_disk
            + std::mem::size_of::<UserKey>()
            + std::mem::size_of::<UserData>();

        let memtable_size = self
            .approx_active_memtable_size
            .fetch_add(size as u32, std::sync::atomic::Ordering::Relaxed);

        drop(memtable_lock);

        if memtable_size > self.config.max_memtable_size {
            log::debug!("Memtable reached threshold size");
            crate::flush::start(self)?;
        }

        Ok(())
    }

    /// Inserts a key-value pair into the tree.
    ///
    /// If the key already exists, the item will be overwritten.
    ///
    /// # Examples
    ///
    /// ```
    /// # let folder = tempfile::tempdir()?;
    /// use lsm_tree::{Config, Tree};
    ///
    /// let tree = Config::new(folder).open()?;
    /// tree.insert("a", nanoid::nanoid!())?;
    /// #
    /// # Ok::<(), lsm_tree::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn insert<K: AsRef<[u8]>, V: AsRef<[u8]>>(&self, key: K, value: V) -> crate::Result<()> {
        let shard = self.journal.lock_shard();

        let value = Value::new(
            key.as_ref(),
            value.as_ref(),
            self.lsn.fetch_add(1, std::sync::atomic::Ordering::AcqRel),
            ValueType::Value,
        );

        self.append_entry(shard, value)?;

        Ok(())
    }

    /// Deletes an item from the tree.
    ///
    /// # Examples
    ///
    /// ```
    /// # let folder = tempfile::tempdir()?;
    /// use lsm_tree::{Config, Tree};
    ///
    /// let tree = Config::new(folder).open()?;
    /// tree.insert("a", "abc")?;
    ///
    /// let item = tree.get("a")?.expect("should have item");
    /// assert_eq!("abc".as_bytes(), &*item);
    ///
    /// tree.remove("a")?;
    ///
    /// let item = tree.get("a")?;
    /// assert_eq!(None, item);
    /// #
    /// # Ok::<(), lsm_tree::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn remove<K: AsRef<[u8]>>(&self, key: K) -> crate::Result<()> {
        let shard = self.journal.lock_shard();

        let value = Value::new(
            key.as_ref(),
            vec![],
            self.lsn.fetch_add(1, std::sync::atomic::Ordering::AcqRel),
            ValueType::Tombstone,
        );

        self.append_entry(shard, value)?;

        Ok(())
    }

    /// Removes the item and returns its value if it was previously in the tree.
    ///
    /// This is less efficient than just deleting because it needs to do a read before deleting.
    ///
    /// # Examples
    ///
    /// ```
    /// # let folder = tempfile::tempdir()?;
    /// use lsm_tree::{Config, Tree};
    ///
    /// let tree = Config::new(folder).open()?;
    ///
    /// let item = tree.remove_entry("a")?;
    /// assert_eq!(None, item);
    ///
    /// tree.insert("a", "abc")?;
    ///
    /// let item = tree.remove_entry("a")?.expect("should have removed item");
    /// assert_eq!("abc".as_bytes(), &*item);
    /// #
    /// # Ok::<(), lsm_tree::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn remove_entry<K: AsRef<[u8]>>(&self, key: K) -> crate::Result<Option<UserData>> {
        self.fetch_update(key, |_| None::<UserData>)
    }

    /// Returns `true` if the tree contains the specified key.
    ///
    /// # Examples
    ///
    /// ```
    /// # let folder = tempfile::tempdir()?;
    /// use lsm_tree::{Config, Tree};
    ///
    /// let tree = Config::new(folder).open()?;
    /// assert!(!tree.contains_key("a")?);
    ///
    /// tree.insert("a", nanoid::nanoid!())?;
    /// assert!(tree.contains_key("a")?);
    /// #
    /// # Ok::<(), lsm_tree::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn contains_key<K: AsRef<[u8]> + std::hash::Hash>(&self, key: K) -> crate::Result<bool> {
        self.get(key).map(|x| x.is_some())
    }

    pub(crate) fn create_iter(&self, seqno: Option<SeqNo>) -> crate::Result<Range<'_>> {
        self.create_range::<UserKey, _>(.., seqno)
    }

    #[allow(clippy::iter_not_returning_iterator)]
    /// Returns an iterator that scans through the entire tree.
    ///
    /// Avoid using this function, or limit it as otherwise it may scan a lot of items.
    ///
    /// # Examples
    ///
    /// ```
    /// # let folder = tempfile::tempdir()?;
    /// use lsm_tree::{Config, Tree};
    ///
    /// let tree = Config::new(folder).open()?;
    ///
    /// tree.insert("a", nanoid::nanoid!())?;
    /// tree.insert("f", nanoid::nanoid!())?;
    /// tree.insert("g", nanoid::nanoid!())?;
    /// assert_eq!(3, tree.iter()?.into_iter().count());
    /// #
    /// # Ok::<(), lsm_tree::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn iter(&self) -> crate::Result<Range<'_>> {
        self.create_iter(None)
    }

    pub(crate) fn create_range<K: AsRef<[u8]>, R: RangeBounds<K>>(
        &self,
        range: R,
        seqno: Option<SeqNo>,
    ) -> crate::Result<Range<'_>> {
        use std::ops::Bound::{self, Excluded, Included, Unbounded};

        let lo: Bound<UserKey> = match range.start_bound() {
            Included(x) => Included(x.as_ref().into()),
            Excluded(x) => Excluded(x.as_ref().into()),
            Unbounded => Unbounded,
        };

        let hi: Bound<UserKey> = match range.end_bound() {
            Included(x) => Included(x.as_ref().into()),
            Excluded(x) => Excluded(x.as_ref().into()),
            Unbounded => Unbounded,
        };

        let bounds: (Bound<UserKey>, Bound<UserKey>) = (lo, hi);

        let lock = self.levels.read().expect("lock is poisoned");

        let segment_info = lock
            .get_all_segments()
            .values()
            .filter(|x| x.check_key_range_overlap(&bounds))
            .cloned()
            .collect::<Vec<_>>();

        Ok(Range::new(
            crate::range::MemTableGuard {
                active: self.active_memtable.read().expect("lock is poisoned"),
                immutable: self.immutable_memtables.read().expect("lock is poisoned"),
            },
            bounds,
            segment_info,
            seqno,
        ))
    }

    /// Returns an iterator over a range of items.
    ///
    /// Avoid using full or unbounded ranges as they may scan a lot of items (unless limited).
    ///
    /// # Examples
    ///
    /// ```
    /// # let folder = tempfile::tempdir()?;
    /// use lsm_tree::{Config, Tree};
    ///
    /// let tree = Config::new(folder).open()?;
    ///
    /// tree.insert("a", nanoid::nanoid!())?;
    /// tree.insert("f", nanoid::nanoid!())?;
    /// tree.insert("g", nanoid::nanoid!())?;
    /// assert_eq!(2, tree.range("a"..="f")?.into_iter().count());
    /// #
    /// # Ok::<(), lsm_tree::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn range<K: AsRef<[u8]>, R: RangeBounds<K>>(&self, range: R) -> crate::Result<Range<'_>> {
        self.create_range(range, None)
    }

    pub(crate) fn create_prefix<K: Into<UserKey>>(
        &self,
        prefix: K,
        seqno: Option<SeqNo>,
    ) -> crate::Result<Prefix<'_>> {
        use std::ops::Bound::{self, Included, Unbounded};

        let prefix = prefix.into();

        let lock = self.levels.read().expect("lock is poisoned");

        let bounds: (Bound<UserKey>, Bound<UserKey>) = (Included(prefix.clone()), Unbounded);

        let segment_info = lock
            .get_all_segments()
            .values()
            .filter(|x| x.check_key_range_overlap(&bounds))
            .cloned()
            .collect();

        Ok(Prefix::new(
            MemTableGuard {
                active: self.active_memtable.read().expect("lock is poisoned"),
                immutable: self.immutable_memtables.read().expect("lock is poisoned"),
            },
            prefix,
            segment_info,
            seqno,
        ))
    }

    /// Returns an iterator over a prefixed set of items.
    ///
    /// Avoid using an empty prefix as it may scan a lot of items (unless limited).
    ///
    /// # Examples
    ///
    /// ```
    /// # let folder = tempfile::tempdir()?;
    /// use lsm_tree::{Config, Tree};
    ///
    /// let tree = Config::new(folder).open()?;
    ///
    /// tree.insert("a", nanoid::nanoid!())?;
    /// tree.insert("ab", nanoid::nanoid!())?;
    /// tree.insert("abc", nanoid::nanoid!())?;
    /// assert_eq!(2, tree.prefix("ab")?.into_iter().count());
    /// #
    /// # Ok::<(), lsm_tree::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn prefix<K: AsRef<[u8]>>(&self, prefix: K) -> crate::Result<Prefix<'_>> {
        self.create_prefix(prefix.as_ref(), None)
    }

    /// Returns the first key-value pair in the tree.
    /// The key in this pair is the minimum key in the tree.
    ///
    /// # Examples
    ///
    /// ```
    /// # use lsm_tree::Error as TreeError;
    /// use lsm_tree::{Tree, Config};
    ///
    /// # let folder = tempfile::tempdir()?;
    /// let tree = Config::new(folder).open()?;
    ///
    /// tree.insert("1", "abc")?;
    /// tree.insert("3", "abc")?;
    /// tree.insert("5", "abc")?;
    ///
    /// let (key, _) = tree.first_key_value()?.expect("item should exist");
    /// assert_eq!(&*key, "1".as_bytes());
    /// #
    /// # Ok::<(), TreeError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn first_key_value(&self) -> crate::Result<Option<(UserKey, UserData)>> {
        self.iter()?.into_iter().next().transpose()
    }

    /// Returns the last key-value pair in the tree.
    /// The key in this pair is the maximum key in the tree.
    ///
    /// # Examples
    ///
    /// ```
    /// # use lsm_tree::Error as TreeError;
    /// use lsm_tree::{Tree, Config};
    ///
    /// # let folder = tempfile::tempdir()?;
    /// let tree = Config::new(folder).open()?;
    ///
    /// tree.insert("1", "abc")?;
    /// tree.insert("3", "abc")?;
    /// tree.insert("5", "abc")?;
    ///
    /// let (key, _) = tree.last_key_value()?.expect("item should exist");
    /// assert_eq!(&*key, "5".as_bytes());
    /// #
    /// # Ok::<(), TreeError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn last_key_value(&self) -> crate::Result<Option<(UserKey, UserData)>> {
        self.iter()?.into_iter().next_back().transpose()
    }

    #[doc(hidden)]
    pub fn get_internal_entry<K: AsRef<[u8]> + std::hash::Hash>(
        &self,
        key: K,
        evict_tombstone: bool,
        seqno: Option<SeqNo>,
    ) -> crate::Result<Option<Value>> {
        let memtable_lock = self.active_memtable.read().expect("lock is poisoned");

        if let Some(item) = memtable_lock.get(&key, seqno) {
            if evict_tombstone {
                return Ok(ignore_tombstone_value(item));
            }
            return Ok(Some(item));
        };
        drop(memtable_lock);

        // Now look in immutable memtables
        let memtable_lock = self.immutable_memtables.read().expect("lock is poisoned");
        for (_, memtable) in memtable_lock.iter().rev() {
            if let Some(item) = memtable.get(&key, seqno) {
                if evict_tombstone {
                    return Ok(ignore_tombstone_value(item));
                }
                return Ok(Some(item));
            }
        }
        drop(memtable_lock);

        // Now look in segments... this may involve disk I/O
        let segment_lock = self.levels.read().expect("lock is poisoned");
        let segments = &segment_lock.get_all_segments_flattened();

        for segment in segments {
            if let Some(item) = segment.get(&key, seqno)? {
                if evict_tombstone {
                    return Ok(ignore_tombstone_value(item));
                }
                return Ok(Some(item));
            }
        }

        Ok(None)
    }

    /// Retrieves an item from the tree.
    ///
    /// # Examples
    ///
    /// ```
    /// # let folder = tempfile::tempdir()?;
    /// use lsm_tree::{Config, Tree};
    ///
    /// let tree = Config::new(folder).open()?;
    /// tree.insert("a", "my_value")?;
    ///
    /// let item = tree.get("a")?;
    /// assert_eq!(Some("my_value".as_bytes().into()), item);
    /// #
    /// # Ok::<(), lsm_tree::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn get<K: AsRef<[u8]> + std::hash::Hash>(&self, key: K) -> crate::Result<Option<UserData>> {
        Ok(self.get_internal_entry(key, true, None)?.map(|x| x.value))
    }

    pub(crate) fn increment_lsn(&self) -> SeqNo {
        self.lsn.fetch_add(1, std::sync::atomic::Ordering::AcqRel)
    }

    /// Compare-and-swap an entry
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn compare_and_swap<K: AsRef<[u8]>>(
        &self,
        key: K,
        expected: Option<&UserData>,
        next: Option<&UserData>,
    ) -> crate::Result<CompareAndSwapResult> {
        let key = key.as_ref();

        let shard = self.journal.lock_shard();
        let seqno = self.increment_lsn();

        match self.get(key)? {
            Some(current_value) => {
                match expected {
                    Some(expected_value) => {
                        // We expected Some and got Some
                        // Check if the value is as expected
                        if current_value != *expected_value {
                            return Ok(Err(CompareAndSwapError {
                                prev: Some(current_value),
                                next: next.cloned(),
                            }));
                        }

                        // Set or delete the object now
                        if let Some(next_value) = next {
                            self.append_entry(
                                shard,
                                Value {
                                    key: key.into(),
                                    value: next_value.clone(),
                                    seqno,
                                    value_type: ValueType::Value,
                                },
                            )?;
                        } else {
                            self.append_entry(
                                shard,
                                Value {
                                    key: key.into(),
                                    value: [].into(),
                                    seqno,
                                    value_type: ValueType::Tombstone,
                                },
                            )?;
                        }

                        Ok(Ok(()))
                    }
                    None => {
                        // We expected Some but got None
                        // CAS error!
                        Ok(Err(CompareAndSwapError {
                            prev: None,
                            next: next.cloned(),
                        }))
                    }
                }
            }
            None => match expected {
                Some(_) => {
                    // We expected Some but got None
                    // CAS error!
                    Ok(Err(CompareAndSwapError {
                        prev: None,
                        next: next.cloned(),
                    }))
                }
                None => match next {
                    // We expected None and got None

                    // Set the object now
                    Some(next_value) => {
                        self.append_entry(
                            shard,
                            Value {
                                key: key.into(),
                                value: next_value.clone(),
                                seqno,
                                value_type: ValueType::Value,
                            },
                        )?;
                        Ok(Ok(()))
                    }
                    // Item is already deleted, do nothing
                    None => Ok(Ok(())),
                },
            },
        }
    }

    /// Atomically fetches and updates an item if it exists.
    ///
    /// Returns the previous value if the item exists.
    ///
    /// # Examples
    ///
    /// ```
    /// # let folder = tempfile::tempdir()?;
    /// use lsm_tree::{Config, Tree};
    ///
    /// let tree = Config::new(folder).open()?;
    /// tree.insert("key", "a")?;
    ///
    /// let prev = tree.fetch_update("key".as_bytes(), |_| Some("b"))?.expect("item should exist");
    /// assert_eq!("a".as_bytes(), &*prev);
    ///
    /// let item = tree.get("key")?.expect("item should exist");
    /// assert_eq!("b".as_bytes(), &*item);
    ///
    /// let prev = tree.fetch_update("key", |_| None::<String>)?.expect("item should exist");
    /// assert_eq!("b".as_bytes(), &*prev);
    ///
    /// assert!(tree.is_empty()?);
    /// #
    /// # Ok::<(), lsm_tree::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn fetch_update<K: AsRef<[u8]>, V: AsRef<[u8]>, F: Fn(Option<&UserData>) -> Option<V>>(
        &self,
        key: K,
        f: F,
    ) -> crate::Result<Option<UserData>> {
        let key = key.as_ref();

        let mut fetched = self.get(key)?;

        loop {
            let expected = fetched.as_ref();
            let next = f(expected).map(|v| v.as_ref().into());

            match self.compare_and_swap(key, expected, next.as_ref())? {
                Ok(()) => return Ok(fetched),
                Err(err) => {
                    fetched = err.prev;
                }
            }
        }
    }

    /// Atomically fetches and updates an item if it exists.
    ///
    /// Returns the updated value if the item exists.
    ///
    /// # Examples
    ///
    /// ```
    /// # let folder = tempfile::tempdir()?;
    /// use lsm_tree::{Config, Tree};
    ///
    /// let tree = Config::new(folder).open()?;
    /// tree.insert("key", "a")?;
    ///
    /// let prev = tree.update_fetch("key", |_| Some("b"))?.expect("item should exist");
    /// assert_eq!("b".as_bytes(), &*prev);
    ///
    /// let item = tree.get("key")?.expect("item should exist");
    /// assert_eq!("b".as_bytes(), &*item);
    ///
    /// let prev = tree.update_fetch("key", |_| None::<String>)?;
    /// assert_eq!(None, prev);
    ///
    /// assert!(tree.is_empty()?);
    /// #
    /// # Ok::<(), lsm_tree::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn update_fetch<K: AsRef<[u8]>, V: AsRef<[u8]>, F: Fn(Option<&UserData>) -> Option<V>>(
        &self,
        key: K,
        f: F,
    ) -> crate::Result<Option<UserData>> {
        let key = key.as_ref();

        let mut fetched = self.get(key)?;

        loop {
            let expected = fetched.as_ref();
            let next = f(expected).map(|v| v.as_ref().into());

            match self.compare_and_swap(key, expected, next.as_ref())? {
                Ok(()) => return Ok(next),
                Err(err) => {
                    fetched = err.prev;
                }
            }
        }
    }

    /// Force-starts a memtable flush thread.
    #[doc(hidden)]
    pub fn force_memtable_flush(
        &self,
    ) -> crate::Result<std::thread::JoinHandle<crate::Result<()>>> {
        crate::flush::start(self)
    }

    /// Force-starts a memtable flush thread and waits until its completely done.
    #[doc(hidden)]
    pub fn wait_for_memtable_flush(&self) -> crate::Result<()> {
        let flush_thread = self.force_memtable_flush()?;
        flush_thread.join().expect("should join")
    }

    /// Performs major compaction.
    #[doc(hidden)]
    #[must_use]
    pub fn do_major_compaction(&self) -> std::thread::JoinHandle<crate::Result<()>> {
        log::info!("Starting major compaction thread");

        let config = self.config();
        let levels = Arc::clone(&self.levels);
        let stop_signal = self.stop_signal.clone();
        let immutable_memtables = Arc::clone(&self.immutable_memtables);
        let open_snapshots = Arc::clone(&self.open_snapshots);
        let block_cache = Arc::clone(&self.block_cache);

        std::thread::spawn(move || {
            let level_lock = levels.write().expect("lock is poisoned");
            let compactor = crate::compaction::major::Strategy::default();
            let choice = compactor.choose(&level_lock);
            drop(level_lock);

            if let crate::compaction::Choice::DoCompact(payload) = choice {
                crate::compaction::worker::do_compaction(
                    config,
                    levels,
                    stop_signal,
                    immutable_memtables,
                    open_snapshots,
                    block_cache,
                    &payload,
                )?;
            }
            Ok(())
        })
    }

    /// Flushes the journal to disk, making sure all written data
    /// is persisted and crash-safe.
    ///
    /// # Examples
    ///
    /// ```
    /// # let folder = tempfile::tempdir()?.into_path();
    /// use lsm_tree::{Config, Tree};
    ///
    /// let tree = Config::new(folder.clone()).open()?;
    /// tree.insert("a", nanoid::nanoid!())?;
    /// tree.flush()?;
    ///
    /// let tree = Config::new(folder).open()?;
    ///
    /// let item = tree.get("a")?;
    /// assert!(item.is_some());
    /// #
    /// # Ok::<(), lsm_tree::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn flush(&self) -> crate::Result<()> {
        self.journal.flush()?;
        Ok(())
    }
}
