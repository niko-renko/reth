//! Support for pruning.

use crate::PrunerError;
use rayon::prelude::*;
use reth_db::{
    abstraction::cursor::{DbCursorRO, DbCursorRW},
    database::Database,
    models::ShardedKey,
    tables,
    transaction::DbTxMut,
    BlockNumberList,
};
use reth_primitives::{
    Address, BlockNumber, ChainSpec, PruneCheckpoint, PruneMode, PruneModes, PrunePart, TxNumber,
};
use reth_provider::{
    BlockReader, DatabaseProviderRW, ProviderFactory, PruneCheckpointReader, PruneCheckpointWriter,
    TransactionsProvider,
};
use std::{ops::RangeInclusive, sync::Arc};
use tracing::{debug, instrument, trace};

/// Result of [Pruner::run] execution
pub type PrunerResult = Result<(), PrunerError>;

/// The pipeline type itself with the result of [Pruner::run]
pub type PrunerWithResult<DB> = (Pruner<DB>, PrunerResult);

pub struct BatchSizes {
    receipts: usize,
    transaction_lookup: usize,
    account_history: usize,
}

impl Default for BatchSizes {
    fn default() -> Self {
        Self { receipts: 10000, transaction_lookup: 10000, account_history: 10000 }
    }
}

/// Pruning routine. Main pruning logic happens in [Pruner::run].
pub struct Pruner<DB> {
    provider_factory: ProviderFactory<DB>,
    /// Minimum pruning interval measured in blocks. All prune parts are checked and, if needed,
    /// pruned, when the chain advances by the specified number of blocks.
    min_block_interval: u64,
    /// Maximum prune depth. Used to determine the pruning target for parts that are needed during
    /// the reorg, e.g. changesets.
    #[allow(dead_code)]
    max_prune_depth: u64,
    /// Last pruned block number. Used in conjunction with `min_block_interval` to determine
    /// when the pruning needs to be initiated.
    last_pruned_block_number: Option<BlockNumber>,
    modes: PruneModes,
    batch_sizes: BatchSizes,
}

impl<DB: Database> Pruner<DB> {
    /// Creates a new [Pruner].
    pub fn new(
        db: DB,
        chain_spec: Arc<ChainSpec>,
        min_block_interval: u64,
        max_prune_depth: u64,
        modes: PruneModes,
        batch_sizes: BatchSizes,
    ) -> Self {
        Self {
            provider_factory: ProviderFactory::new(db, chain_spec),
            min_block_interval,
            max_prune_depth,
            last_pruned_block_number: None,
            modes,
            batch_sizes,
        }
    }

    /// Run the pruner
    pub fn run(&mut self, tip_block_number: BlockNumber) -> PrunerResult {
        let provider = self.provider_factory.provider_rw()?;

        if let Some((to_block, prune_mode)) =
            self.modes.prune_target_block_receipts(tip_block_number)?
        {
            self.prune_receipts(&provider, to_block, prune_mode)?;
        }

        if let Some((to_block, prune_mode)) =
            self.modes.prune_target_block_transaction_lookup(tip_block_number)?
        {
            self.prune_transaction_lookup(&provider, to_block, prune_mode)?;
        }

        if let Some((to_block, prune_mode)) =
            self.modes.prune_target_block_account_history(tip_block_number)?
        {
            self.prune_account_history(&provider, to_block, prune_mode)?;
        }

        provider.commit()?;

        self.last_pruned_block_number = Some(tip_block_number);
        Ok(())
    }

    /// Returns `true` if the pruning is needed at the provided tip block number.
    /// This determined by the check against minimum pruning interval and last pruned block number.
    pub fn is_pruning_needed(&self, tip_block_number: BlockNumber) -> bool {
        if self.last_pruned_block_number.map_or(true, |last_pruned_block_number| {
            // Saturating subtraction is needed for the case when the chain was reverted, meaning
            // current block number might be less than the previously pruned block number. If
            // that's the case, no pruning is needed as outdated data is also reverted.
            tip_block_number.saturating_sub(last_pruned_block_number) >= self.min_block_interval
        }) {
            debug!(
                target: "pruner",
                last_pruned_block_number = ?self.last_pruned_block_number,
                %tip_block_number,
                "Minimum pruning interval reached"
            );
            true
        } else {
            false
        }
    }

    /// Get next inclusive tx number range to prune according to the checkpoint and `to_block` block
    /// number.
    ///
    /// To get the range start:
    /// 1. If checkpoint exists, get next block body and return its first tx number.
    /// 2. If checkpoint doesn't exist, return 0.
    ///
    /// To get the range end: get last tx number for the provided `to_block`.
    fn get_next_tx_num_range_from_checkpoint(
        &self,
        provider: &DatabaseProviderRW<'_, DB>,
        prune_part: PrunePart,
        to_block: BlockNumber,
    ) -> reth_interfaces::Result<Option<RangeInclusive<TxNumber>>> {
        let from_tx_num = provider
            .get_prune_checkpoint(prune_part)?
            .map(|checkpoint| provider.block_body_indices(checkpoint.block_number + 1))
            .transpose()?
            .flatten()
            .map(|body| body.first_tx_num)
            .unwrap_or_default();

        let to_tx_num = match provider.block_body_indices(to_block)? {
            Some(body) => body,
            None => return Ok(None),
        }
        .last_tx_num();

        Ok(Some(from_tx_num..=to_tx_num))
    }

    /// Prune receipts up to the provided block, inclusive.
    #[instrument(level = "trace", skip(self, provider), target = "pruner")]
    fn prune_receipts(
        &self,
        provider: &DatabaseProviderRW<'_, DB>,
        to_block: BlockNumber,
        prune_mode: PruneMode,
    ) -> PrunerResult {
        let range = match self.get_next_tx_num_range_from_checkpoint(
            provider,
            PrunePart::Receipts,
            to_block,
        )? {
            Some(range) => range,
            None => {
                trace!(target: "pruner", "No receipts to prune");
                return Ok(())
            }
        };
        let total = range.clone().count();

        let mut processed = 0;
        provider.prune_table_with_iterator_in_batches::<tables::Receipts, _>(
            range,
            self.batch_sizes.receipts,
            |entries| {
                processed += entries;
                trace!(
                    target: "pruner",
                    %entries,
                    progress = format!("{:.1}%", 100.0 * processed as f64 / total as f64),
                    "Pruned receipts"
                );
            },
        )?;

        provider.save_prune_checkpoint(
            PrunePart::Receipts,
            PruneCheckpoint { block_number: to_block, prune_mode },
        )?;

        Ok(())
    }

    /// Prune transaction lookup entries up to the provided block, inclusive.
    #[instrument(level = "trace", skip(self, provider), target = "pruner")]
    fn prune_transaction_lookup(
        &self,
        provider: &DatabaseProviderRW<'_, DB>,
        to_block: BlockNumber,
        prune_mode: PruneMode,
    ) -> PrunerResult {
        let range = match self.get_next_tx_num_range_from_checkpoint(
            provider,
            PrunePart::TransactionLookup,
            to_block,
        )? {
            Some(range) => range,
            None => {
                trace!(target: "pruner", "No receipts to prune");
                return Ok(())
            }
        };
        let last_tx_num = *range.end();
        let total = range.clone().count();
        let mut processed = 0;

        for i in range.step_by(self.batch_sizes.transaction_lookup) {
            // The `min` ensures that the transaction range doesn't exceed the last transaction
            // number. `last_tx_num + 1` is used to include the last transaction in the range.
            let tx_range = i..(i + self.batch_sizes.transaction_lookup as u64).min(last_tx_num + 1);

            // Retrieve transactions in the range and calculate their hashes in parallel
            let mut hashes = provider
                .transactions_by_tx_range(tx_range.clone())?
                .into_par_iter()
                .map(|transaction| transaction.hash())
                .collect::<Vec<_>>();

            // Number of transactions retrieved from the database should match the tx range count
            let tx_count = tx_range.clone().count();
            if hashes.len() != tx_count {
                return Err(PrunerError::InconsistentData(
                    "Unexpected number of transaction hashes retrieved by transaction number range",
                ))
            }

            // Pre-sort hashes to prune them in order
            hashes.sort_unstable();

            let entries = provider.prune_table_with_iterator::<tables::TxHashNumber, _>(hashes)?;
            processed += entries;
            trace!(
                target: "pruner",
                %entries,
                progress = format!("{:.1}%", 100.0 * processed as f64 / total as f64),
                "Pruned transaction lookup"
            );
        }

        provider.save_prune_checkpoint(
            PrunePart::TransactionLookup,
            PruneCheckpoint { block_number: to_block, prune_mode },
        )?;

        Ok(())
    }

    /// Prune account history up to the provided block, inclusive.
    #[instrument(level = "trace", skip(self, provider), target = "pruner")]
    fn prune_account_history(
        &self,
        provider: &DatabaseProviderRW<'_, DB>,
        to_block: BlockNumber,
        prune_mode: PruneMode,
    ) -> PrunerResult {
        let from_block = provider
            .get_prune_checkpoint(PrunePart::AccountHistory)?
            .map(|checkpoint| checkpoint.block_number + 1)
            .unwrap_or_default();
        let range = from_block..=to_block;
        let total = range.clone().count();

        let mut processed = 0;
        provider.prune_table_with_range_in_batches::<tables::AccountChangeSet, _>(
            range,
            self.batch_sizes.account_history,
            |entries| {
                processed += entries;
                trace!(
                    target: "pruner",
                    %entries,
                    progress = format!("{:.1}%", 100.0 * processed as f64 / total as f64),
                    "Pruned account history (changesets)"
                );
            },
        )?;

        let mut cursor = provider.tx_ref().cursor_write::<tables::AccountHistory>()?;
        while let Some(result) = cursor.next()? {
            let (key, blocks): (ShardedKey<Address>, BlockNumberList) = result;

            if key.highest_block_number <= to_block {
                // If shard consists only of block numbers less than the target one, delete shard
                // completely.
                cursor.delete_current()?;
                if key.highest_block_number == to_block {
                    // Shard contains only block numbers up to the target one, so we can skip to the
                    // next address. It is guaranteed that further shards for this address will not
                    // contain the target block number, as it's in this shard.
                    cursor.seek_exact(ShardedKey::last(key.key))?;
                }
            } else {
                // Shard contains block numbers that are higher than the target one, so we need to
                // filter it. It is guaranteed that further shards for this address will not contain
                // the target block number, as it's in this shard.
                let blocks = blocks
                    .iter(0)
                    .skip_while(|block| *block <= to_block as usize)
                    .collect::<Vec<_>>();
                cursor.upsert(key.clone(), BlockNumberList::new_pre_sorted(blocks))?;

                // Jump to the next address
                cursor.seek_exact(ShardedKey::last(key.key))?;
            }

            processed += 1;
            if processed % self.batch_sizes.account_history == 0 {
                trace!(
                    target: "pruner",
                    entries = self.batch_sizes.account_history,
                    "Pruned account history (indices)"
                );
            }
        }

        if processed % self.batch_sizes.account_history != 0 {
            trace!(
                target: "pruner",
                entries = processed % self.batch_sizes.account_history,
                "Pruned account history (indices)"
            );
        }

        provider.save_prune_checkpoint(
            PrunePart::AccountHistory,
            PruneCheckpoint { block_number: to_block, prune_mode },
        )?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{pruner::BatchSizes, Pruner};
    use assert_matches::assert_matches;
    use reth_db::{tables, test_utils::create_test_rw_db, BlockNumberList};
    use reth_interfaces::test_utils::{
        generators,
        generators::{
            random_block_range, random_changeset_range, random_eoa_account_range, random_receipt,
        },
    };
    use reth_primitives::{
        Address, BlockNumber, PruneCheckpoint, PruneMode, PruneModes, PrunePart, H256, MAINNET,
    };
    use reth_provider::PruneCheckpointReader;
    use reth_stages::test_utils::TestTransaction;
    use std::{collections::BTreeMap, ops::AddAssign};

    #[test]
    fn is_pruning_needed() {
        let db = create_test_rw_db();
        let pruner =
            Pruner::new(db, MAINNET.clone(), 5, 0, PruneModes::default(), BatchSizes::default());

        // No last pruned block number was set before
        let first_block_number = 1;
        assert!(pruner.is_pruning_needed(first_block_number));

        // Delta is not less than min block interval
        let second_block_number = first_block_number + pruner.min_block_interval;
        assert!(pruner.is_pruning_needed(second_block_number));

        // Delta is less than min block interval
        let third_block_number = second_block_number;
        assert!(pruner.is_pruning_needed(third_block_number));
    }

    #[test]
    fn prune_receipts() {
        let tx = TestTransaction::default();
        let mut rng = generators::rng();

        let blocks = random_block_range(&mut rng, 0..=100, H256::zero(), 0..10);
        tx.insert_blocks(blocks.iter(), None).expect("insert blocks");

        let mut receipts = Vec::new();
        for block in &blocks {
            for transaction in &block.body {
                receipts
                    .push((receipts.len() as u64, random_receipt(&mut rng, transaction, Some(0))));
            }
        }
        tx.insert_receipts(receipts).expect("insert receipts");

        assert_eq!(
            tx.table::<tables::Transactions>().unwrap().len(),
            blocks.iter().map(|block| block.body.len()).sum::<usize>()
        );
        assert_eq!(
            tx.table::<tables::Transactions>().unwrap().len(),
            tx.table::<tables::Receipts>().unwrap().len()
        );

        let test_prune = |to_block: BlockNumber| {
            let prune_mode = PruneMode::Before(to_block);
            let pruner = Pruner::new(
                tx.inner_raw(),
                MAINNET.clone(),
                5,
                0,
                PruneModes { receipts: Some(prune_mode), ..Default::default() },
                BatchSizes {
                    // Less than total amount of blocks to prune to test the batching logic
                    receipts: 10,
                    ..Default::default()
                },
            );

            let provider = tx.inner_rw();
            assert_matches!(pruner.prune_receipts(&provider, to_block, prune_mode), Ok(()));
            provider.commit().expect("commit");

            assert_eq!(
                tx.table::<tables::Receipts>().unwrap().len(),
                blocks[to_block as usize + 1..].iter().map(|block| block.body.len()).sum::<usize>()
            );
            assert_eq!(
                tx.inner().get_prune_checkpoint(PrunePart::Receipts).unwrap(),
                Some(PruneCheckpoint { block_number: to_block, prune_mode })
            );
        };

        // Pruning first time ever, no previous checkpoint is present
        test_prune(10);
        // Prune second time, previous checkpoint is present, should continue pruning from where
        // ended last time
        test_prune(20);
    }

    #[test]
    fn prune_transaction_lookup() {
        let tx = TestTransaction::default();
        let mut rng = generators::rng();

        let blocks = random_block_range(&mut rng, 0..=100, H256::zero(), 0..10);
        tx.insert_blocks(blocks.iter(), None).expect("insert blocks");

        let mut tx_hash_numbers = Vec::new();
        for block in &blocks {
            for transaction in &block.body {
                tx_hash_numbers.push((transaction.hash, tx_hash_numbers.len() as u64));
            }
        }
        tx.insert_tx_hash_numbers(tx_hash_numbers).expect("insert tx hash numbers");

        assert_eq!(
            tx.table::<tables::Transactions>().unwrap().len(),
            blocks.iter().map(|block| block.body.len()).sum::<usize>()
        );
        assert_eq!(
            tx.table::<tables::Transactions>().unwrap().len(),
            tx.table::<tables::TxHashNumber>().unwrap().len()
        );

        let test_prune = |to_block: BlockNumber| {
            let prune_mode = PruneMode::Before(to_block);
            let pruner = Pruner::new(
                tx.inner_raw(),
                MAINNET.clone(),
                5,
                0,
                PruneModes { transaction_lookup: Some(prune_mode), ..Default::default() },
                BatchSizes {
                    // Less than total amount of blocks to prune to test the batching logic
                    transaction_lookup: 10,
                    ..Default::default()
                },
            );

            let provider = tx.inner_rw();
            assert_matches!(
                pruner.prune_transaction_lookup(&provider, to_block, prune_mode),
                Ok(())
            );
            provider.commit().expect("commit");

            assert_eq!(
                tx.table::<tables::TxHashNumber>().unwrap().len(),
                blocks[to_block as usize + 1..].iter().map(|block| block.body.len()).sum::<usize>()
            );
            assert_eq!(
                tx.inner().get_prune_checkpoint(PrunePart::TransactionLookup).unwrap(),
                Some(PruneCheckpoint { block_number: to_block, prune_mode })
            );
        };

        // Pruning first time ever, no previous checkpoint is present
        test_prune(10);
        // Prune second time, previous checkpoint is present, should continue pruning from where
        // ended last time
        test_prune(20);
    }

    #[test]
    fn prune_account_history() {
        let tx = TestTransaction::default();
        let mut rng = generators::rng();

        let block_num = 7000;
        let blocks = random_block_range(&mut rng, 0..=block_num, H256::zero(), 0..1);
        tx.insert_blocks(blocks.iter(), None).expect("insert blocks");

        let accounts =
            random_eoa_account_range(&mut rng, 0..3).into_iter().collect::<BTreeMap<_, _>>();

        let (changesets, _) = random_changeset_range(
            &mut rng,
            blocks.iter(),
            accounts.clone().into_iter().map(|(addr, acc)| (addr, (acc, Vec::new()))),
            0..0,
            0..0,
        );
        tx.insert_changesets(changesets.clone(), None).expect("insert changesets");
        tx.insert_history(changesets.clone(), None).expect("insert history");

        let account_occurrences = tx.table::<tables::AccountHistory>().unwrap().into_iter().fold(
            BTreeMap::<Address, usize>::new(),
            |mut map, (key, _)| {
                map.entry(key.key).or_default().add_assign(1);
                map
            },
        );
        assert!(account_occurrences.into_iter().any(|(_, occurrences)| occurrences > 1));

        assert_eq!(
            tx.table::<tables::AccountChangeSet>().unwrap().len(),
            changesets.iter().flatten().count()
        );

        let original_shards = tx.table::<tables::AccountHistory>().unwrap();

        let test_prune = |to_block: BlockNumber| {
            let prune_mode = PruneMode::Before(to_block);
            let pruner = Pruner::new(
                tx.inner_raw(),
                MAINNET.clone(),
                5,
                0,
                PruneModes { account_history: Some(prune_mode), ..Default::default() },
                BatchSizes {
                    // Less than total amount of blocks to prune to test the batching logic
                    account_history: 10,
                    ..Default::default()
                },
            );

            let provider = tx.inner_rw();
            assert_matches!(pruner.prune_account_history(&provider, to_block, prune_mode), Ok(()));
            provider.commit().expect("commit");

            assert_eq!(
                tx.table::<tables::AccountChangeSet>().unwrap().len(),
                changesets[to_block as usize + 1..].iter().flatten().count()
            );

            let actual_shards = tx.table::<tables::AccountHistory>().unwrap();

            let expected_shards = original_shards
                .iter()
                .filter(|(key, _)| key.highest_block_number > to_block)
                .map(|(key, blocks)| {
                    let new_blocks = blocks
                        .iter(0)
                        .skip_while(|block| *block <= to_block as usize)
                        .collect::<Vec<_>>();
                    (key.clone(), BlockNumberList::new_pre_sorted(new_blocks))
                })
                .collect::<Vec<_>>();

            assert_eq!(actual_shards, expected_shards);

            assert_eq!(
                tx.inner().get_prune_checkpoint(PrunePart::AccountHistory).unwrap(),
                Some(PruneCheckpoint { block_number: to_block, prune_mode })
            );
        };

        // Prune first time: no previous checkpoint is present
        test_prune(3000);
        // Prune second time: previous checkpoint is present, should continue pruning from where
        // ended last time
        test_prune(4500);
    }
}
