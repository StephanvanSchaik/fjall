use crate::{Tree, Value};

/// An atomic write batch
pub struct Batch {
    data: Vec<Value>,
    tree: Tree,
}

impl Batch {
    /// Initializes a new write batch
    /// This function is called by Tree.batch()
    pub(crate) fn new(tree: Tree) -> Self {
        Self {
            data: Vec::with_capacity(100),
            tree,
        }
    }

    /// Inserts a key-value pair into the batch
    pub fn insert<K: Into<Vec<u8>>, V: Into<Vec<u8>>>(&mut self, key: K, value: V) {
        self.data
            .push(Value::new(key.into(), value.into(), false, 0));
    }

    /// Adds a tombstone marker for a key
    pub fn remove<K: Into<Vec<u8>>>(&mut self, key: K) {
        self.data.push(Value::new(key.into(), vec![], true, 0));
    }

    /// Commits the batch to the LSM-tree atomically
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs
    pub fn commit(mut self) -> crate::Result<()> {
        let mut shard = self.tree.journal.lock_shard();

        let batch_seqno = self
            .tree
            .lsn
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        for item in &mut self.data {
            item.seqno = batch_seqno;
        }

        let bytes_written = shard.write_batch(self.data.clone())?;
        shard.flush()?;

        // TODO: size adjustments
        /* memtable.size_in_bytes += bytes_written as u32;

        trace!("Applying {} batched items to memtable", self.data.len());
        for entry in std::mem::take(&mut self.data) {
            memtable.insert(entry);
        } */

        // TODO: check
        /* if memtable.exceeds_threshold(self.tree.config.max_memtable_size) {
            crate::flush::start(&self.tree, commit_log, memtable)?;
        } */

        Ok(())
    }
}
