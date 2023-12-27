use super::{marker::Marker, writer::JournalWriter};
use crate::batch::Item as BatchItem;
use crate::journal::reader::JournalShardReader;
use lsm_tree::{serde::Serializable, MemTable, SeqNo};
use std::{
    collections::HashMap,
    fs::OpenOptions,
    path::{Path, PathBuf},
    sync::Arc,
};

// TODO: strategy, skip invalid batches (CRC or invalid item length) or throw error
/// Errors that can occur during journal recovery
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryError {
    /// Batch had less items than expected, so it's incomplete
    InsufficientLength,

    /// Batch was not terminated, so it's possibly incomplete
    MissingTerminator,

    /// Too many items in batch
    TooManyItems,

    /// The CRC value does not match the expected value
    CrcCheck,
}

// TODO: don't require locking for sync check
pub struct JournalShard {
    pub(crate) path: PathBuf,
    pub(crate) writer: JournalWriter,
    pub(crate) should_sync: bool,
}

impl JournalShard {
    pub fn rotate<P: AsRef<Path>>(&mut self, path: P) -> crate::Result<()> {
        self.should_sync = false;
        self.writer.rotate(path)
    }

    pub fn create_new<P: AsRef<Path>>(path: P) -> crate::Result<Self> {
        Ok(Self {
            path: path.as_ref().to_path_buf(),
            writer: JournalWriter::create_new(path)?,
            should_sync: bool::default(),
        })
    }

    pub fn from_file<P: AsRef<Path>>(path: P) -> crate::Result<Self> {
        Ok(Self {
            path: path.as_ref().to_path_buf(),
            writer: JournalWriter::from_file(path)?,
            should_sync: bool::default(),
        })
    }

    /// Recovers a journal shard and writes the items into the given memtable
    ///
    /// Will truncate the file to the position of the last valid batch
    pub fn recover_and_repair<P: AsRef<Path>>(
        path: P,
        memtables: &mut HashMap<Arc<str>, MemTable>,
        whitelist: Option<&[Arc<str>]>,
    ) -> crate::Result<()> {
        let path = path.as_ref();
        let recoverer = JournalShardReader::new(path)?;

        let mut hasher = crc32fast::Hasher::new();
        let mut is_in_batch = false;
        let mut batch_counter = 0;
        let mut batch_seqno = SeqNo::default();
        let mut last_valid_pos = 0;

        let mut items: Vec<BatchItem> = vec![];

        'a: for item in recoverer {
            let (journal_file_pos, item) = item?;

            match item {
                Marker::Start { item_count, seqno } => {
                    if is_in_batch {
                        log::warn!("Invalid batch: found batch start inside batch");

                        // Discard batch
                        log::warn!("Truncating shard to {last_valid_pos}");
                        let file = OpenOptions::new().write(true).open(path)?;
                        file.set_len(last_valid_pos)?;
                        file.sync_all()?;

                        break 'a;
                    }

                    is_in_batch = true;
                    batch_counter = item_count;
                    batch_seqno = seqno;
                }
                Marker::End(checksum) => {
                    if batch_counter > 0 {
                        log::error!("Invalid batch: insufficient length");
                        return Err(crate::Error::JournalRecovery(
                            RecoveryError::InsufficientLength,
                        ));
                    }

                    if !is_in_batch {
                        log::error!("Invalid batch: found end marker without start marker");

                        // Discard batch
                        log::warn!("Truncating shard to {last_valid_pos}");
                        let file = OpenOptions::new().write(true).open(path)?;
                        file.set_len(last_valid_pos)?;
                        file.sync_all()?;

                        break 'a;
                    }

                    eprintln!("=====");
                    for item in &items {
                        eprintln!("{item:?}");
                    }

                    let crc = hasher.finalize();
                    if crc != checksum {
                        log::error!("Invalid batch: checksum check failed, expected: {checksum}, got: {crc}");
                        return Err(crate::Error::JournalRecovery(RecoveryError::CrcCheck));
                    }

                    // Reset all variables
                    hasher = crc32fast::Hasher::new();
                    is_in_batch = false;
                    batch_counter = 0;

                    // NOTE: Clippy says into_iter() is better
                    // but in this case probably not
                    #[allow(clippy::iter_with_drain)]
                    for item in items.drain(..) {
                        if let Some(whitelist) = whitelist {
                            if !whitelist.contains(&item.partition) {
                                continue;
                            }
                        }

                        let memtable = memtables.entry(item.partition).or_default();

                        let value = lsm_tree::Value {
                            key: item.key,
                            value: item.value,
                            seqno: batch_seqno,
                            value_type: item.value_type,
                        };

                        memtable.insert(value);
                    }

                    last_valid_pos = journal_file_pos;
                }
                Marker::Item {
                    partition,
                    key,
                    value,
                    value_type,
                } => {
                    let item = Marker::Item {
                        partition: partition.clone(),
                        key: key.clone(),
                        value: value.clone(),
                        value_type,
                    };
                    let mut bytes = Vec::with_capacity(100);
                    item.serialize(&mut bytes)?;

                    hasher.update(&bytes);

                    if !is_in_batch {
                        log::warn!("Invalid batch: found end marker without start marker");

                        // Discard batch
                        log::warn!("Truncating shard to {last_valid_pos}");
                        let file = OpenOptions::new().write(true).open(path)?;
                        file.set_len(last_valid_pos)?;
                        file.sync_all()?;

                        break 'a;
                    }

                    if batch_counter == 0 {
                        log::error!("Invalid batch: Expected end marker (too many items in batch)");
                        return Err(crate::Error::JournalRecovery(RecoveryError::TooManyItems));
                    }

                    batch_counter -= 1;

                    items.push(BatchItem {
                        partition,
                        key,
                        value,
                        value_type,
                    });
                }
            }
        }

        if is_in_batch {
            log::warn!("Invalid batch: missing terminator, but last batch, so probably incomplete, discarding to keep atomicity");

            // Discard batch
            log::warn!("Truncating shard to {last_valid_pos}");
            let file = OpenOptions::new().write(true).open(path)?;
            file.set_len(last_valid_pos)?;
            file.sync_all()?;
        }

        Ok(())
    }
}
