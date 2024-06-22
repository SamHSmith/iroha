//! Module with queue actor
use core::time::Duration;
use std::num::NonZeroUsize;

use crossbeam_queue::ArrayQueue;
use dashmap::{mapref::entry::Entry, DashMap};
use eyre::Result;
use indexmap::IndexSet;
use iroha_config::parameters::actual::Queue as Config;
use iroha_crypto::HashOf;
use iroha_data_model::{
    account::AccountId,
    events::pipeline::{TransactionEvent, TransactionStatus},
    transaction::prelude::*,
};
use iroha_logger::{trace, warn};
use iroha_primitives::time::TimeSource;
use rand::seq::IteratorRandom;
use thiserror::Error;

use crate::{prelude::*, EventsSender};

impl AcceptedTransaction {
    // TODO: We should have another type of transaction like `CheckedTransaction` in the type system?
    #[must_use]
    fn check_signature_condition(&self, state_view: &StateView<'_>) -> bool {
        let authority = self.as_ref().authority();

        let transaction_signatories = self
            .as_ref()
            .signatures()
            .iter()
            .map(|signature| signature.public_key())
            .cloned()
            .collect();

        state_view
            .world
            .map_account(authority, |account| {
                account.check_signature_check_condition(&transaction_signatories)
            })
            .unwrap_or(false)
    }

    /// Check if [`self`] is committed or rejected.
    fn is_in_blockchain(&self, state_view: &StateView<'_>) -> bool {
        state_view.has_transaction(self.as_ref().hash())
    }
}

/// Lockfree queue for transactions
///
/// Multiple producers, single consumer
#[derive(Debug)]
pub struct Queue {
    events_sender: EventsSender,
    /// The queue for transactions
    tx_hashes: ArrayQueue<HashOf<SignedTransaction>>,
    /// [`AcceptedTransaction`]s addressed by `Hash`
    accepted_txs: DashMap<HashOf<SignedTransaction>, AcceptedTransaction>,
    /// Amount of transactions per user in the queue
    txs_per_user: DashMap<AccountId, usize>,
    /// The maximum number of transactions in the queue
    capacity: NonZeroUsize,
    /// The maximum number of transactions in the queue per user. Used to apply throttling
    capacity_per_user: NonZeroUsize,
    /// The time source used to check transaction against
    ///
    /// A mock time source is used in tests for determinism
    time_source: TimeSource,
    /// Length of time after which transactions are dropped.
    pub tx_time_to_live: Duration,
    /// A point in time that is considered `Future` we cannot use
    /// current time, because of network time synchronisation issues
    future_threshold: Duration,
}

/// Queue push error
#[derive(Error, Copy, Clone, Debug, displaydoc::Display)]
#[allow(variant_size_differences)]
pub enum Error {
    /// Queue is full
    Full,
    /// Transaction is regarded to have been tampered to have a future timestamp
    InFuture,
    /// Transaction expired
    Expired,
    /// Transaction is already applied
    InBlockchain,
    /// User reached maximum number of transactions in the queue
    MaximumTransactionsPerUser,
    /// The transaction is already in the queue
    IsInQueue,
    /// Failure during signature condition execution
    SignatureCondition,
}

/// Failure that can pop up when pushing transaction into the queue
#[derive(Debug)]
pub struct Failure {
    /// Transaction failed to be pushed into the queue
    pub tx: AcceptedTransaction,
    /// Push failure reason
    pub err: Error,
}

impl Queue {
    /// Makes queue from configuration
    pub fn from_config(cfg: Config, events_sender: EventsSender) -> Self {
        Self {
            events_sender,
            tx_hashes: ArrayQueue::new(cfg.capacity.get()),
            accepted_txs: DashMap::new(),
            txs_per_user: DashMap::new(),
            capacity: cfg.capacity,
            capacity_per_user: cfg.capacity_per_user,
            time_source: TimeSource::new_system(),
            tx_time_to_live: cfg.transaction_time_to_live,
            future_threshold: cfg.future_threshold,
        }
    }

    fn is_pending(&self, tx: &AcceptedTransaction, state_view: &StateView, time_padding: Duration) -> bool {
        !self.is_expired(tx, time_padding) && !tx.is_in_blockchain(state_view)
    }

    /// Checks if the transaction is waiting longer than its TTL or than the TTL from [`Config`].
    pub fn is_expired(&self, tx: &AcceptedTransaction, time_padding: Duration) -> bool {
        let tx_creation_time = tx.as_ref().creation_time();

        let time_limit = tx.as_ref().time_to_live().map_or_else(
            || self.tx_time_to_live,
            |tx_time_to_live| core::cmp::min(self.tx_time_to_live, tx_time_to_live),
        );

        let curr_time = self.time_source.get_unix_time();
        curr_time.saturating_sub(tx_creation_time) + time_padding > time_limit
    }

    /// If `true`, this transaction is regarded to have been tampered to have a future timestamp.
    fn is_in_future(&self, tx: &AcceptedTransaction) -> bool {
        let tx_timestamp = tx.as_ref().creation_time();
        let curr_time = self.time_source.get_unix_time();
        tx_timestamp.saturating_sub(curr_time) > self.future_threshold
    }

    /// Returns all pending transactions.
    pub fn all_transactions<'state>(
        &'state self,
        state_view: &'state StateView,
    ) -> impl Iterator<Item = AcceptedTransaction> + 'state {
        self.accepted_txs.iter().filter_map(|tx| {
            if self.is_pending(tx.value(), state_view, Duration::from_secs(0)) {
                return Some(tx.value().clone());
            }

            None
        })
    }

    /// Returns `n` randomly selected transaction from the queue.
    pub fn n_random_transactions(
        &self,
        n: u32,
        state_view: &StateView,
    ) -> Vec<AcceptedTransaction> {
        self.accepted_txs
            .iter()
            .filter(|e| self.is_pending(e.value(), state_view, Duration::from_secs(0)))
            .map(|e| e.value().clone())
            .choose_multiple(
                &mut rand::thread_rng(),
                n.try_into().expect("u32 should always fit in usize"),
            )
    }

    fn check_tx(&self, tx: &AcceptedTransaction, state_view: &StateView, time_padding: Duration) -> Result<(), Error> {
        if self.is_in_future(tx) {
            Err(Error::InFuture)
        } else if self.is_expired(tx, time_padding) {
            Err(Error::Expired)
        } else if tx.is_in_blockchain(state_view) {
            Err(Error::InBlockchain)
        } else if !tx.check_signature_condition(state_view) {
            Err(Error::SignatureCondition)
        } else {
            Ok(())
        }
    }

    /// Push transaction into queue.
    ///
    /// # Errors
    /// See [`enum@Error`]
    pub fn push(&self, tx: AcceptedTransaction, state_view: &StateView) -> Result<(), Failure> {
        trace!(?tx, "Pushing to the queue");
        if let Err(err) = self.check_tx(&tx, state_view, Duration::from_secs(0)) {
            return Err(Failure { tx, err });
        }

        // Get `txs_len` before entry to avoid deadlock
        let txs_len = self.accepted_txs.len();
        let hash = tx.as_ref().hash();
        let entry = match self.accepted_txs.entry(hash) {
            Entry::Occupied(_) => {
                return Err(Failure {
                    tx,
                    err: Error::IsInQueue,
                })
            }
            Entry::Vacant(entry) => entry,
        };

        if txs_len >= self.capacity.get() {
            warn!(
                max = self.capacity,
                "Achieved maximum amount of transactions"
            );
            return Err(Failure {
                tx,
                err: Error::Full,
            });
        }

        if let Err(err) = self.check_and_increase_per_user_tx_count(tx.as_ref().authority()) {
            return Err(Failure { tx, err });
        }

        // Insert entry first so that the `tx` popped from `queue` will always have a `(hash, tx)` record in `txs`.
        entry.insert(tx);
        self.tx_hashes.push(hash).map_err(|err_hash| {
            warn!("Queue is full");
            let (_, err_tx) = self
                .accepted_txs
                .remove(&err_hash)
                .expect("Inserted just before match");
            self.decrease_per_user_tx_count(err_tx.as_ref().authority());
            Failure {
                tx: err_tx,
                err: Error::Full,
            }
        })?;
        let _ = self.events_sender.send(
            TransactionEvent {
                hash,
                block_height: None,
                status: TransactionStatus::Queued,
            }
            .into(),
        );
        trace!("Transaction queue length = {}", self.tx_hashes.len(),);
        Ok(())
    }

    /// Pop single transaction from the queue. Removes all transactions that fail the `tx_check`.
    fn pop_from_queue(
        &self,
        seen: &mut Vec<HashOf<SignedTransaction>>,
        state_view: &StateView,
        expired_transactions: &mut Vec<AcceptedTransaction>,
        time_padding: Duration,
    ) -> Option<AcceptedTransaction> {
        loop {
            let hash = self.tx_hashes.pop()?;

            let entry = match self.accepted_txs.entry(hash) {
                Entry::Occupied(entry) => entry,
                // FIXME: Reachable under high load. Investigate, see if it's a problem.
                // As practice shows this code is not `unreachable!()`.
                // When transactions are submitted quickly it can be reached.
                Entry::Vacant(_) => {
                    warn!("Looks like we're experiencing a high load");
                    continue;
                }
            };

            let tx = entry.get();
            if let Err(e) = self.check_tx(tx, state_view, time_padding) {
                let (_, tx) = entry.remove_entry();
                self.decrease_per_user_tx_count(tx.as_ref().authority());
                if let Error::Expired = e {
                    expired_transactions.push(tx);
                }
                continue;
            }

            seen.push(hash);
            return Some(tx.clone());
        }
    }

    /// Return the number of transactions in the queue.
    pub fn tx_len(&self) -> usize {
        self.accepted_txs.len()
    }

    /// Gets transactions till they fill whole block or till the end of queue.
    ///
    /// BEWARE: Shouldn't be called in parallel with itself.
    #[cfg(test)]
    fn collect_transactions_for_block(
        &self,
        state_view: &StateView,
        max_txs_in_block: usize,
    ) -> Vec<AcceptedTransaction> {
        let mut transactions = Vec::with_capacity(max_txs_in_block);
        self.get_transactions_for_block(state_view, max_txs_in_block, &mut transactions);
        transactions
    }

    /// Put transactions into provided vector until they fill the whole block or there are no more transactions in the queue.
    ///
    /// BEWARE: Shouldn't be called in parallel with itself.
    pub fn get_transactions_for_block(
        &self,
        state_view: &StateView,
        max_txs_in_block: usize,
        transactions: &mut Vec<AcceptedTransaction>,
        time_padding: Duration,
    ) {
        if transactions.len() >= max_txs_in_block {
            return;
        }

        let mut seen_queue = Vec::new();
        let mut expired_transactions = Vec::new();

        let txs_from_queue = core::iter::from_fn(|| {
            self.pop_from_queue(&mut seen_queue, state_view, &mut expired_transactions, time_padding)
        });

        let transactions_hashes: IndexSet<HashOf<SignedTransaction>> =
            transactions.iter().map(|tx| tx.as_ref().hash()).collect();
        let txs = txs_from_queue
            .filter(|tx| !transactions_hashes.contains(&tx.as_ref().hash()))
            .take(max_txs_in_block - transactions.len());
        transactions.extend(txs);

        seen_queue
            .into_iter()
            .try_for_each(|hash| self.tx_hashes.push(hash))
            .expect("Exceeded the number of transactions pending");

        expired_transactions
            .into_iter()
            .map(|tx| TransactionEvent {
                hash: tx.as_ref().hash(),
                block_height: None,
                status: TransactionStatus::Expired,
            })
            .for_each(|e| {
                let _ = self.events_sender.send(e.into());
            });
    }

    /// Check that the user adhered to the maximum transaction per user limit and increment their transaction count.
    fn check_and_increase_per_user_tx_count(&self, account_id: &AccountId) -> Result<(), Error> {
        match self.txs_per_user.entry(account_id.clone()) {
            Entry::Vacant(vacant) => {
                vacant.insert(1);
            }
            Entry::Occupied(mut occupied) => {
                let txs = *occupied.get();
                if txs >= self.capacity_per_user.get() {
                    warn!(
                        max_txs_per_user = self.capacity_per_user,
                        %account_id,
                        "Account reached maximum allowed number of transactions in the queue per user"
                    );
                    return Err(Error::MaximumTransactionsPerUser);
                }
                *occupied.get_mut() += 1;
            }
        }

        Ok(())
    }

    fn decrease_per_user_tx_count(&self, account_id: &AccountId) {
        let Entry::Occupied(mut occupied) = self.txs_per_user.entry(account_id.clone()) else {
            panic!("Call to decrease always should be paired with increase count. This is a bug.")
        };

        let count = occupied.get_mut();
        if *count > 1 {
            *count -= 1;
        } else {
            occupied.remove_entry();
        }
    }
}

#[cfg(test)]
// this is `pub` to re-use internal utils
pub mod tests {
    use std::{str::FromStr, sync::Arc, thread, time::Duration};

    use iroha_data_model::{prelude::*, transaction::TransactionLimits};
    use nonzero_ext::nonzero;
    use rand::Rng as _;
    use tokio::test;

    use super::*;
    use crate::{
        kura::Kura,
        query::store::LiveQueryStore,
        smartcontracts::isi::Registrable as _,
        state::{State, World},
        PeersIds,
    };

    impl Queue {
        pub fn test(cfg: Config, time_source: &TimeSource) -> Self {
            Self {
                events_sender: tokio::sync::broadcast::Sender::new(1),
                tx_hashes: ArrayQueue::new(cfg.capacity.get()),
                accepted_txs: DashMap::new(),
                txs_per_user: DashMap::new(),
                capacity: cfg.capacity,
                capacity_per_user: cfg.capacity_per_user,
                time_source: time_source.clone(),
                tx_time_to_live: cfg.transaction_time_to_live,
                future_threshold: cfg.future_threshold,
            }
        }
    }

    fn accepted_tx(
        account_id: &str,
        key: &KeyPair,
        time_source: &TimeSource,
    ) -> AcceptedTransaction {
        let chain_id = ChainId::from("0");

        let message = std::iter::repeat_with(rand::random::<char>)
            .take(16)
            .collect();
        let instructions = [Fail { message }];
        let tx = TransactionBuilder::new_with_time_source(
            chain_id.clone(),
            AccountId::from_str(account_id).expect("Valid"),
            time_source,
        )
        .with_instructions(instructions)
        .sign(key);
        let limits = TransactionLimits {
            max_instruction_number: 4096,
            max_wasm_size_bytes: 0,
        };
        AcceptedTransaction::accept(tx, &chain_id, &limits).expect("Failed to accept Transaction.")
    }

    pub fn world_with_test_domains(
        signatories: impl IntoIterator<Item = iroha_crypto::PublicKey>,
    ) -> World {
        let domain_id = DomainId::from_str("wonderland").expect("Valid");
        let account_id = AccountId::from_str("alice@wonderland").expect("Valid");
        let mut domain = Domain::new(domain_id).build(&account_id);
        let mut signatories = signatories.into_iter();
        let mut account = Account::new(account_id.clone(), signatories.next().unwrap());
        for signatory in signatories {
            account = account.add_signatory(signatory);
        }
        let account = account.build(&account_id);
        assert!(domain.add_account(account).is_none());
        World::with([domain], PeersIds::new())
    }

    fn config_factory() -> Config {
        Config {
            transaction_time_to_live: Duration::from_secs(100),
            capacity: 100.try_into().unwrap(),
            ..Config::default()
        }
    }

    #[test]
    async fn push_tx() {
        let key_pair = KeyPair::random();
        let kura = Kura::blank_kura_for_testing();
        let query_handle = LiveQueryStore::test().start();
        let state = Arc::new(State::new(
            world_with_test_domains([key_pair.public_key().clone()]),
            kura,
            query_handle,
        ));
        let state_view = state.view();

        let (_time_handle, time_source) = TimeSource::new_mock(Duration::default());

        let queue = Queue::test(config_factory(), &time_source);

        queue
            .push(
                accepted_tx("alice@wonderland", &key_pair, &time_source),
                &state_view,
            )
            .expect("Failed to push tx into queue");
    }

    #[test]
    async fn push_tx_overflow() {
        let capacity = nonzero!(10_usize);

        let key_pair = KeyPair::random();
        let kura = Kura::blank_kura_for_testing();
        let query_handle = LiveQueryStore::test().start();
        let state = Arc::new(State::new(
            world_with_test_domains([key_pair.public_key().clone()]),
            kura,
            query_handle,
        ));
        let state_view = state.view();

        let (time_handle, time_source) = TimeSource::new_mock(Duration::default());

        let queue = Queue::test(
            Config {
                transaction_time_to_live: Duration::from_secs(100),
                capacity,
                ..Config::default()
            },
            &time_source,
        );

        for _ in 0..capacity.get() {
            queue
                .push(
                    accepted_tx("alice@wonderland", &key_pair, &time_source),
                    &state_view,
                )
                .expect("Failed to push tx into queue");
            time_handle.advance(Duration::from_millis(10));
        }

        assert!(matches!(
            queue.push(
                accepted_tx("alice@wonderland", &key_pair, &time_source),
                &state_view
            ),
            Err(Failure {
                err: Error::Full,
                ..
            })
        ));
    }

    #[test]
    async fn push_multisignature_tx() {
        let chain_id = ChainId::from("0");

        let key_pairs = [KeyPair::random(), KeyPair::random()];
        let kura = Kura::blank_kura_for_testing();
        let state = {
            let domain_id = DomainId::from_str("wonderland").expect("Valid");
            let account_id = AccountId::from_str("alice@wonderland").expect("Valid");
            let mut domain = Domain::new(domain_id).build(&account_id);
            let mut account = Account::new(account_id.clone(), key_pairs[0].public_key().clone())
                .add_signatory(key_pairs[1].public_key().clone())
                .build(&account_id);
            account.signature_check_condition = SignatureCheckCondition::all_account_signatures();
            assert!(domain.add_account(account).is_none());
            let query_handle = LiveQueryStore::test().start();
            Arc::new(State::new(
                World::with([domain], PeersIds::new()),
                kura,
                query_handle,
            ))
        };
        let state_view = state.view();

        let (_time_handle, time_source) = TimeSource::new_mock(Duration::default());

        let queue = Queue::test(config_factory(), &time_source);
        let instructions: [InstructionBox; 0] = [];
        let tx = TransactionBuilder::new_with_time_source(
            chain_id.clone(),
            "alice@wonderland".parse().expect("Valid"),
            &time_source,
        )
        .with_instructions(instructions);
        let tx_limits = TransactionLimits {
            max_instruction_number: 4096,
            max_wasm_size_bytes: 0,
        };
        let fully_signed_tx: AcceptedTransaction = {
            let mut signed_tx = tx.clone().sign(&key_pairs[0]);
            for key_pair in &key_pairs[1..] {
                signed_tx = signed_tx.sign(key_pair);
            }
            AcceptedTransaction::accept(signed_tx, &chain_id, &tx_limits)
                .expect("Failed to accept Transaction.")
        };
        // Check that fully signed transaction passes signature check
        assert!(fully_signed_tx.check_signature_condition(&state_view));

        let get_tx = |key_pair| {
            AcceptedTransaction::accept(tx.clone().sign(&key_pair), &chain_id, &tx_limits)
                .expect("Failed to accept Transaction.")
        };
        for key_pair in key_pairs {
            let partially_signed_tx: AcceptedTransaction = get_tx(key_pair);
            // Check that none of partially signed txs passes signature check
            assert!(!partially_signed_tx.check_signature_condition(&state_view),);
            assert!(matches!(
                queue
                    .push(partially_signed_tx, &state_view)
                    .unwrap_err()
                    .err,
                Error::SignatureCondition
            ))
        }
    }

    #[test]
    async fn get_available_txs() {
        let max_txs_in_block = 2;
        let alice_key = KeyPair::random();
        let kura = Kura::blank_kura_for_testing();
        let query_handle = LiveQueryStore::test().start();
        let state = Arc::new(State::new(
            world_with_test_domains([alice_key.public_key().clone()]),
            kura,
            query_handle,
        ));
        let state_view = state.view();

        let (time_handle, time_source) = TimeSource::new_mock(Duration::default());

        let queue = Queue::test(
            Config {
                transaction_time_to_live: Duration::from_secs(100),
                ..config_factory()
            },
            &time_source,
        );
        for _ in 0..5 {
            queue
                .push(
                    accepted_tx("alice@wonderland", &alice_key, &time_source),
                    &state_view,
                )
                .expect("Failed to push tx into queue");
            time_handle.advance(Duration::from_millis(10));
        }

        let available = queue.collect_transactions_for_block(&state_view, max_txs_in_block);
        assert_eq!(available.len(), max_txs_in_block);
    }

    #[test]
    async fn push_tx_already_in_blockchain() {
        let alice_key = KeyPair::random();
        let kura = Kura::blank_kura_for_testing();
        let query_handle = LiveQueryStore::test().start();
        let state = State::new(
            world_with_test_domains([alice_key.public_key().clone()]),
            kura,
            query_handle,
        );

        let (_time_handle, time_source) = TimeSource::new_mock(Duration::default());

        let tx = accepted_tx("alice@wonderland", &alice_key, &time_source);
        let mut state_block = state.block();
        state_block.transactions.insert(tx.as_ref().hash(), 1);
        state_block.commit();
        let state_view = state.view();
        let queue = Queue::test(config_factory(), &time_source);
        assert!(matches!(
            queue.push(tx, &state_view),
            Err(Failure {
                err: Error::InBlockchain,
                ..
            })
        ));
        assert_eq!(queue.accepted_txs.len(), 0);
    }

    #[test]
    async fn get_tx_drop_if_in_blockchain() {
        let max_txs_in_block = 2;
        let alice_key = KeyPair::random();
        let kura = Kura::blank_kura_for_testing();
        let query_handle = LiveQueryStore::test().start();
        let state = State::new(
            world_with_test_domains([alice_key.public_key().clone()]),
            kura,
            query_handle,
        );

        let (_time_handle, time_source) = TimeSource::new_mock(Duration::default());

        let tx = accepted_tx("alice@wonderland", &alice_key, &time_source);
        let queue = Queue::test(config_factory(), &time_source);
        queue.push(tx.clone(), &state.view()).unwrap();
        let mut state_block = state.block();
        state_block.transactions.insert(tx.as_ref().hash(), 1);
        state_block.commit();
        assert_eq!(
            queue
                .collect_transactions_for_block(&state.view(), max_txs_in_block)
                .len(),
            0
        );
        assert_eq!(queue.accepted_txs.len(), 0);
    }

    #[test]
    async fn get_available_txs_with_timeout() {
        let max_txs_in_block = 6;
        let alice_key = KeyPair::random();
        let kura = Kura::blank_kura_for_testing();
        let query_handle = LiveQueryStore::test().start();
        let state = Arc::new(State::new(
            world_with_test_domains([alice_key.public_key().clone()]),
            kura,
            query_handle,
        ));
        let state_view = state.view();

        let (time_handle, time_source) = TimeSource::new_mock(Duration::default());

        let queue = Queue::test(
            Config {
                transaction_time_to_live: Duration::from_millis(200),
                ..config_factory()
            },
            &time_source,
        );
        for _ in 0..(max_txs_in_block - 1) {
            queue
                .push(
                    accepted_tx("alice@wonderland", &alice_key, &time_source),
                    &state_view,
                )
                .expect("Failed to push tx into queue");
            time_handle.advance(Duration::from_millis(100));
        }

        queue
            .push(
                accepted_tx("alice@wonderland", &alice_key, &time_source),
                &state_view,
            )
            .expect("Failed to push tx into queue");
        time_handle.advance(Duration::from_millis(101));
        assert_eq!(
            queue
                .collect_transactions_for_block(&state_view, max_txs_in_block)
                .len(),
            1
        );

        queue
            .push(
                accepted_tx("alice@wonderland", &alice_key, &time_source),
                &state_view,
            )
            .expect("Failed to push tx into queue");
        time_handle.advance(Duration::from_millis(210));
        assert_eq!(
            queue
                .collect_transactions_for_block(&state_view, max_txs_in_block)
                .len(),
            0
        );
    }

    // Queue should only drop transactions which are already committed or ttl expired.
    // Others should stay in the queue until that moment.
    #[test]
    async fn transactions_available_after_pop() {
        let max_txs_in_block = 2;
        let alice_key = KeyPair::random();
        let kura = Kura::blank_kura_for_testing();
        let query_handle = LiveQueryStore::test().start();
        let state = Arc::new(State::new(
            world_with_test_domains([alice_key.public_key().clone()]),
            kura,
            query_handle,
        ));
        let state_view = state.view();

        let (_time_handle, time_source) = TimeSource::new_mock(Duration::default());

        let queue = Queue::test(config_factory(), &time_source);
        queue
            .push(
                accepted_tx("alice@wonderland", &alice_key, &time_source),
                &state_view,
            )
            .expect("Failed to push tx into queue");

        let a = queue
            .collect_transactions_for_block(&state_view, max_txs_in_block)
            .into_iter()
            .map(|tx| tx.as_ref().hash())
            .collect::<Vec<_>>();
        let b = queue
            .collect_transactions_for_block(&state_view, max_txs_in_block)
            .into_iter()
            .map(|tx| tx.as_ref().hash())
            .collect::<Vec<_>>();
        assert_eq!(a.len(), 1);
        assert_eq!(a, b);
    }

    #[test]
    async fn custom_expired_transaction_is_rejected() {
        const TTL_MS: u64 = 200;

        let chain_id = ChainId::from("0");

        let max_txs_in_block = 2;
        let alice_key = KeyPair::random();
        let kura = Kura::blank_kura_for_testing();
        let query_handle = LiveQueryStore::test().start();
        let state = Arc::new(State::new(
            world_with_test_domains([alice_key.public_key().clone()]),
            kura,
            query_handle,
        ));
        let state_view = state.view();

        let (time_handle, time_source) = TimeSource::new_mock(Duration::default());
        let mut queue = Queue::test(config_factory(), &time_source);
        let (event_sender, mut event_receiver) = tokio::sync::broadcast::channel(1);
        queue.events_sender = event_sender;
        let instructions = [Fail {
            message: "expired".to_owned(),
        }];
        let mut tx = TransactionBuilder::new_with_time_source(
            chain_id.clone(),
            AccountId::from_str("alice@wonderland").expect("Valid"),
            &time_source,
        )
        .with_instructions(instructions);
        tx.set_ttl(Duration::from_millis(TTL_MS));
        let tx = tx.sign(&alice_key);
        let limits = TransactionLimits {
            max_instruction_number: 4096,
            max_wasm_size_bytes: 0,
        };
        let tx_hash = tx.hash();
        let tx = AcceptedTransaction::accept(tx, &chain_id, &limits)
            .expect("Failed to accept Transaction.");
        queue
            .push(tx.clone(), &state_view)
            .expect("Failed to push tx into queue");
        let queued_tx_event = event_receiver.recv().await.unwrap();

        assert_eq!(
            queued_tx_event,
            TransactionEvent {
                hash: tx_hash,
                block_height: None,
                status: TransactionStatus::Queued,
            }
            .into()
        );

        let mut txs = Vec::new();
        time_handle.advance(Duration::from_millis(TTL_MS + 1));
        queue.get_transactions_for_block(&state_view, max_txs_in_block, &mut txs);
        let expired_tx_event = event_receiver.recv().await.unwrap();
        assert!(txs.is_empty());

        assert_eq!(
            expired_tx_event,
            TransactionEvent {
                hash: tx_hash,
                block_height: None,
                status: TransactionStatus::Expired,
            }
            .into()
        )
    }

    #[test]
    async fn concurrent_stress_test() {
        let max_txs_in_block = 10;
        let alice_key = KeyPair::random();
        let kura = Kura::blank_kura_for_testing();
        let query_handle = LiveQueryStore::test().start();
        let state = Arc::new(State::new(
            world_with_test_domains([alice_key.public_key().clone()]),
            kura,
            query_handle,
        ));

        let (time_handle, time_source) = TimeSource::new_mock(Duration::default());

        let queue = Arc::new(Queue::test(
            Config {
                transaction_time_to_live: Duration::from_secs(100),
                capacity: 100_000_000.try_into().unwrap(),
                ..Config::default()
            },
            &time_source,
        ));

        let start_time = std::time::Instant::now();
        let run_for = Duration::from_secs(5);

        let push_txs_handle = {
            let queue_arc_clone = Arc::clone(&queue);
            let state = state.clone();

            // Spawn a thread where we push transactions
            thread::spawn(move || {
                while start_time.elapsed() < run_for {
                    let tx = accepted_tx("alice@wonderland", &alice_key, &time_source);
                    match queue_arc_clone.push(tx, &state.view()) {
                        Ok(())
                        | Err(Failure {
                            err: Error::Full | Error::MaximumTransactionsPerUser,
                            ..
                        }) => (),
                        Err(Failure { err, .. }) => panic!("{err}"),
                    }
                }
            })
        };

        // Spawn a thread where we get_transactions_for_block and add them to state
        let get_txs_handle = {
            let queue = Arc::clone(&queue);

            thread::spawn(move || {
                while start_time.elapsed() < run_for {
                    for tx in queue.collect_transactions_for_block(&state.view(), max_txs_in_block)
                    {
                        let mut state_block = state.block();
                        state_block.transactions.insert(tx.as_ref().hash(), 1);
                        state_block.commit();
                    }
                    // Simulate random small delays
                    let delay = Duration::from_millis(rand::thread_rng().gen_range(0..25));
                    thread::sleep(delay);
                    time_handle.advance(delay);
                }
            })
        };

        push_txs_handle.join().unwrap();
        get_txs_handle.join().unwrap();

        // Validate the queue state.
        let array_queue: Vec<_> = core::iter::from_fn(|| queue.tx_hashes.pop()).collect();

        assert_eq!(array_queue.len(), queue.accepted_txs.len());
        for tx in array_queue {
            assert!(queue.accepted_txs.contains_key(&tx));
        }
    }

    #[test]
    async fn push_tx_in_future() {
        let future_threshold = Duration::from_secs(1);

        let alice_id = "alice@wonderland";
        let alice_key = KeyPair::random();
        let kura = Kura::blank_kura_for_testing();
        let query_handle = LiveQueryStore::test().start();
        let state = Arc::new(State::new(
            world_with_test_domains([alice_key.public_key().clone()]),
            kura,
            query_handle,
        ));
        let state_view = state.view();

        let (time_handle, time_source) = TimeSource::new_mock(Duration::default());
        let queue = Queue::test(
            Config {
                future_threshold,
                ..Config::default()
            },
            &time_source,
        );

        let tx = accepted_tx(alice_id, &alice_key, &time_source);
        assert!(queue.push(tx.clone(), &state_view).is_ok());

        // create the same tx but with timestamp in the future
        time_handle.advance(future_threshold * 2);
        let tx = accepted_tx(alice_id, &alice_key, &time_source);
        time_handle.rewind(future_threshold * 2);

        assert!(matches!(
            queue.push(tx, &state_view),
            Err(Failure {
                err: Error::InFuture,
                ..
            })
        ));
        assert_eq!(queue.accepted_txs.len(), 1);
    }

    #[test]
    async fn queue_throttling() {
        let alice_key_pair = KeyPair::random();
        let bob_key_pair = KeyPair::random();
        let kura = Kura::blank_kura_for_testing();
        let world = {
            let domain_id = DomainId::from_str("wonderland").expect("Valid");
            let alice_account_id = AccountId::from_str("alice@wonderland").expect("Valid");
            let bob_account_id = AccountId::from_str("bob@wonderland").expect("Valid");
            let mut domain = Domain::new(domain_id).build(&alice_account_id);
            let alice_account = Account::new(
                alice_account_id.clone(),
                alice_key_pair.public_key().clone(),
            )
            .build(&alice_account_id);
            let bob_account =
                Account::new(bob_account_id.clone(), bob_key_pair.public_key().clone())
                    .build(&bob_account_id);
            assert!(domain.add_account(alice_account).is_none());
            assert!(domain.add_account(bob_account).is_none());
            World::with([domain], PeersIds::new())
        };
        let query_handle = LiveQueryStore::test().start();
        let state = State::new(world, kura, query_handle);

        let (_time_handle, time_source) = TimeSource::new_mock(Duration::default());

        let queue = Queue::test(
            Config {
                transaction_time_to_live: Duration::from_secs(100),
                capacity: 100.try_into().unwrap(),
                capacity_per_user: 1.try_into().unwrap(),
                ..Config::default()
            },
            &time_source,
        );

        // First push by Alice should be fine
        queue
            .push(
                accepted_tx("alice@wonderland", &alice_key_pair, &time_source),
                &state.view(),
            )
            .expect("Failed to push tx into queue");

        // Second push by Alice excide limit and will be rejected
        let result = queue.push(
            accepted_tx("alice@wonderland", &alice_key_pair, &time_source),
            &state.view(),
        );
        assert!(
            matches!(
                result,
                Err(Failure {
                    tx: _,
                    err: Error::MaximumTransactionsPerUser
                }),
            ),
            "Failed to match: {result:?}",
        );

        // First push by Bob should be fine despite previous Alice error
        queue
            .push(
                accepted_tx("bob@wonderland", &bob_key_pair, &time_source),
                &state.view(),
            )
            .expect("Failed to push tx into queue");

        let transactions = queue.collect_transactions_for_block(&state.view(), 10);
        assert_eq!(transactions.len(), 2);
        let mut state_block = state.block();
        for transaction in transactions {
            // Put transaction hashes into state as if they were in the blockchain
            state_block
                .transactions
                .insert(transaction.as_ref().hash(), 1);
        }
        state_block.commit();
        // Cleanup transactions
        let transactions = queue.collect_transactions_for_block(&state.view(), 10);
        assert!(transactions.is_empty());

        // After cleanup Alice and Bob pushes should work fine
        queue
            .push(
                accepted_tx("alice@wonderland", &alice_key_pair, &time_source),
                &state.view(),
            )
            .expect("Failed to push tx into queue");

        queue
            .push(
                accepted_tx("bob@wonderland", &bob_key_pair, &time_source),
                &state.view(),
            )
            .expect("Failed to push tx into queue");
    }
}
