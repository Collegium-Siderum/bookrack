// SPDX-License-Identifier: Apache-2.0

//! The node-id partition allocator.
//!
//! Every book owns a contiguous, exclusive block of the global node-id
//! space — its *partition*. The arithmetic of the partition protocol
//! lives in `bookrack_core`; this module is its persistent side: the
//! `node_id_partitions` table that records which intake owns which
//! partition and how far each partition's id cursor has advanced.

use bookrack_core::{NODE_CAPACITY, NodeId, PartitionIdx};
use rusqlite::{OptionalExtension, Row};

use crate::{Corpus, CorpusError, Result};

/// One allocated partition: a book's reservation of the node-id space.
#[derive(Debug, Clone)]
pub struct Partition {
    /// This partition's index in the global id space.
    pub idx: PartitionIdx,
    /// The book's root node id — local offset 1 of the partition.
    pub book_root_id: NodeId,
    /// The intake (source file) this partition was allocated for.
    pub intake_id: i64,
    /// The next local offset the allocator will hand out. Offset 1 is
    /// the root, so a freshly allocated partition starts at 2.
    pub next_local_id: i64,
    /// When the partition was allocated, as an ISO-8601 UTC timestamp.
    pub allocated_at: String,
}

impl Partition {
    /// Build a [`Partition`] from a `node_id_partitions` row. Column
    /// order must match the `SELECT` lists in this module.
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Partition> {
        Ok(Partition {
            idx: PartitionIdx::new(row.get(0)?),
            book_root_id: NodeId::new(row.get(1)?),
            intake_id: row.get(2)?,
            next_local_id: row.get(3)?,
            allocated_at: row.get(4)?,
        })
    }
}

impl Corpus {
    /// Allocate the partition for `intake_id`.
    ///
    /// The partition index *is* the intake id. Intake and partition are
    /// one-to-one — one source file, one intake, one book — and an
    /// intake id is a never-reused surrogate key, so keying the
    /// partition by it makes a book's partition (and therefore every
    /// node id in the book) reproduce identically across a rebuild. A
    /// counter-allocated index would instead drift whenever an earlier
    /// intake was removed before a rebuild. `intake_id` must be a
    /// positive `catalog.intake` id; intake ids start at 1, so index 0
    /// stays free as a sentinel and a zero node id never names a book.
    ///
    /// The root node itself is not created here — only its id is
    /// reserved, at local offset 1 — because the root carries a node
    /// type and title that only the caller knows.
    ///
    /// Allocation happens exactly once per intake: a second call for an
    /// intake that already owns a partition fails with
    /// [`CorpusError::PartitionAlreadyAllocated`]. Re-ingesting a book
    /// means removing it first, then allocating anew.
    pub fn allocate_partition(&mut self, intake_id: i64) -> Result<Partition> {
        let idx = PartitionIdx::new(intake_id);
        let book_root_id = idx.root();

        let tx = self.conn.transaction()?;
        let already: Option<i64> = tx
            .query_row(
                "SELECT partition_idx FROM node_id_partitions WHERE intake_id = ?1",
                [intake_id],
                |row| row.get(0),
            )
            .optional()?;
        if already.is_some() {
            return Err(CorpusError::PartitionAlreadyAllocated(intake_id));
        }

        // The freshly allocated partition starts its cursor at offset 2,
        // immediately past the reserved root. The timestamp is generated
        // by SQLite so the whole crate has one timestamp source.
        let allocated_at: String = tx.query_row(
            "INSERT INTO node_id_partitions
               (partition_idx, book_root_id, intake_id, next_local_id, allocated_at)
             VALUES (?1, ?2, ?3, 2, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
             RETURNING allocated_at",
            (idx.get(), book_root_id.get(), intake_id),
            |row| row.get(0),
        )?;
        tx.commit()?;

        Ok(Partition {
            idx,
            book_root_id,
            intake_id,
            next_local_id: 2,
            allocated_at,
        })
    }

    /// Look up the partition owning `intake_id`, or `None` if the intake
    /// has no partition yet.
    pub fn partition_for_intake(&self, intake_id: i64) -> Result<Option<Partition>> {
        let partition = self
            .conn
            .query_row(
                "SELECT partition_idx, book_root_id, intake_id, next_local_id, allocated_at
                 FROM node_id_partitions WHERE intake_id = ?1",
                [intake_id],
                Partition::from_row,
            )
            .optional()?;
        Ok(partition)
    }

    /// Reserve `count` consecutive node ids in `partition` and return
    /// them in ascending order.
    ///
    /// The ids are reserved by advancing the partition's persistent
    /// cursor, so concurrent or later calls never collide. Fails with
    /// [`CorpusError::PartitionExhausted`] if the partition cannot fit
    /// that many further nodes, and with
    /// [`CorpusError::UnknownPartition`] if it was never allocated. A
    /// `count` of zero reserves nothing and returns an empty vector.
    pub fn allocate_node_ids(
        &mut self,
        partition: PartitionIdx,
        count: u32,
    ) -> Result<Vec<NodeId>> {
        if count == 0 {
            return Ok(Vec::new());
        }
        let tx = self.conn.transaction()?;
        let next: i64 = tx
            .query_row(
                "SELECT next_local_id FROM node_id_partitions WHERE partition_idx = ?1",
                [partition.get()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(CorpusError::UnknownPartition(partition))?;

        let last = next + i64::from(count) - 1;
        if last > NODE_CAPACITY {
            return Err(CorpusError::PartitionExhausted {
                partition,
                requested: count,
            });
        }
        tx.execute(
            "UPDATE node_id_partitions SET next_local_id = ?1 WHERE partition_idx = ?2",
            (last + 1, partition.get()),
        )?;
        tx.commit()?;

        // The bounds check above guarantees every offset is in range,
        // so composition never returns `None`.
        let ids = (next..=last)
            .map(|offset| {
                partition
                    .node_id(offset)
                    .expect("offset is within NODE_CAPACITY")
            })
            .collect();
        Ok(ids)
    }

    /// Delete a book outright: its whole node tree and its allocator
    /// row. Idempotent — dropping an absent partition is a no-op — so a
    /// removal interrupted partway can simply be re-run.
    ///
    /// This is the `corpus.db` step of a cross-store removal; the caller
    /// is responsible for the vector store and `catalog.db`.
    pub fn drop_partition(&mut self, partition: PartitionIdx) -> Result<()> {
        let tx = self.conn.transaction()?;
        // Every node of the book shares the root id in `book_root_id`,
        // so one indexed delete clears the entire tree.
        tx.execute(
            "DELETE FROM nodes WHERE book_root_id = ?1",
            [partition.root().get()],
        )?;
        tx.execute(
            "DELETE FROM node_id_partitions WHERE partition_idx = ?1",
            [partition.get()],
        )?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_partition_is_keyed_by_its_intake_id() {
        let mut corpus = Corpus::open_in_memory().expect("open");

        // The partition index is the intake id itself, so allocation
        // order is irrelevant and the mapping survives any rebuild.
        let first = corpus.allocate_partition(10).expect("allocate");
        assert_eq!(first.idx, PartitionIdx::new(10));
        assert_eq!(first.book_root_id, PartitionIdx::new(10).root());
        assert_eq!(first.next_local_id, 2);
        assert!(!first.allocated_at.is_empty());

        // A later, smaller intake id still maps to its own index — the
        // allocator never derives the index from a running counter.
        let second = corpus.allocate_partition(3).expect("allocate");
        assert_eq!(second.idx, PartitionIdx::new(3));
    }

    #[test]
    fn an_intake_may_own_only_one_partition() {
        let mut corpus = Corpus::open_in_memory().expect("open");
        corpus.allocate_partition(7).expect("first allocate");
        let err = corpus
            .allocate_partition(7)
            .expect_err("second allocate must fail");
        assert!(matches!(err, CorpusError::PartitionAlreadyAllocated(7)));
    }

    #[test]
    fn partition_lookup_finds_allocated_and_misses_absent() {
        let mut corpus = Corpus::open_in_memory().expect("open");
        let allocated = corpus.allocate_partition(42).expect("allocate");

        let found = corpus
            .partition_for_intake(42)
            .expect("lookup")
            .expect("present");
        assert_eq!(found.idx, allocated.idx);
        assert_eq!(found.intake_id, 42);

        assert!(corpus.partition_for_intake(999).expect("lookup").is_none());
    }

    #[test]
    fn node_ids_are_handed_out_consecutively_past_the_root() {
        let mut corpus = Corpus::open_in_memory().expect("open");
        let p = corpus.allocate_partition(1).expect("allocate").idx;

        // First batch starts at offset 2, immediately after the root.
        let first = corpus.allocate_node_ids(p, 3).expect("allocate ids");
        assert_eq!(
            first,
            vec![
                p.node_id(2).unwrap(),
                p.node_id(3).unwrap(),
                p.node_id(4).unwrap(),
            ]
        );

        // A second batch continues where the first stopped.
        let second = corpus.allocate_node_ids(p, 2).expect("allocate ids");
        assert_eq!(second, vec![p.node_id(5).unwrap(), p.node_id(6).unwrap()]);

        assert_eq!(
            corpus
                .partition_for_intake(1)
                .unwrap()
                .unwrap()
                .next_local_id,
            7
        );
    }

    #[test]
    fn allocating_zero_ids_is_a_no_op() {
        let mut corpus = Corpus::open_in_memory().expect("open");
        let p = corpus.allocate_partition(1).expect("allocate").idx;
        assert!(corpus.allocate_node_ids(p, 0).expect("allocate").is_empty());
        assert_eq!(
            corpus
                .partition_for_intake(1)
                .unwrap()
                .unwrap()
                .next_local_id,
            2,
            "a zero request must not move the cursor"
        );
    }

    #[test]
    fn allocating_in_an_unknown_partition_is_rejected() {
        let mut corpus = Corpus::open_in_memory().expect("open");
        let err = corpus
            .allocate_node_ids(PartitionIdx::new(5), 1)
            .expect_err("must fail");
        assert!(matches!(err, CorpusError::UnknownPartition(_)));
    }

    #[test]
    fn a_partition_cannot_overflow_its_capacity() {
        let mut corpus = Corpus::open_in_memory().expect("open");
        let p = corpus.allocate_partition(1).expect("allocate").idx;
        // The cursor sits at offset 2, so NODE_CAPACITY ids would reach
        // offset NODE_CAPACITY + 1 — one past the partition's last slot.
        let err = corpus
            .allocate_node_ids(p, NODE_CAPACITY as u32)
            .expect_err("must overflow");
        assert!(matches!(err, CorpusError::PartitionExhausted { .. }));
    }

    #[test]
    fn dropping_a_partition_frees_its_allocator_row_and_is_idempotent() {
        let mut corpus = Corpus::open_in_memory().expect("open");
        let p = corpus.allocate_partition(1).expect("allocate").idx;

        corpus.drop_partition(p).expect("first drop");
        assert!(corpus.partition_for_intake(1).expect("lookup").is_none());
        // Re-running a removal that already completed must not fail.
        corpus.drop_partition(p).expect("second drop is a no-op");
    }
}
