use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::warn;

use crate::manager::types::{
    AccountModel, AccountPartitionRef, AccountSnapshot, BalanceQuery, BalanceView, BalancesDisplay,
    CollateralScope, CollateralSummary, MarginMode, MarginStateView,
};
use crate::traits::AccountError;
use crate::types::{Currency, Exchange};

#[derive(Debug, Clone, Error)]
pub enum AccountManagerError {
    #[error("unsupported account model {requested:?} for exchange {exchange:?}")]
    UnsupportedAccountModel {
        exchange: Exchange,
        requested: AccountModel,
    },

    #[error("partition not found on {exchange:?}: {partition}")]
    PartitionNotFound {
        exchange: Exchange,
        partition: String,
    },

    #[error("asset balance not found on {exchange:?} for {asset} ({partition})")]
    AssetBalanceNotFound {
        exchange: Exchange,
        asset: Currency,
        partition: String,
    },

    #[error(
        "snapshot for {exchange:?} is stale: updated at {last_update_utc}, max age {max_age_secs}s"
    )]
    SnapshotStale {
        exchange: Exchange,
        last_update_utc: DateTime<Utc>,
        max_age_secs: i64,
    },

    #[error("invalid account query: {reason}")]
    InvalidQuery { reason: String },

    #[error("account manager error: {reason}")]
    Other { reason: String },

    #[error("account manager actor mailbox is closed")]
    ActorMailboxClosed,

    #[error("account manager actor response channel dropped")]
    ActorResponseDropped,

    #[error("account source error: {0}")]
    SourceError(#[from] AccountError),
}

/// Configuration for the AccountManagerActor.
#[derive(Debug, Clone)]
pub struct AccountManagerConfig {
    /// Size of the actor's command mailbox.
    pub mailbox_capacity: usize,
    /// Snapshots older than this duration are considered stale; a background refresh is triggered
    /// automatically while still serving the cached data.
    pub snapshot_max_age: chrono::Duration,
}

impl Default for AccountManagerConfig {
    fn default() -> Self {
        Self {
            mailbox_capacity: 64,
            snapshot_max_age: chrono::Duration::seconds(30),
        }
    }
}

/// Internal result type for background snapshot fetches.
struct FetchResult {
    exchange: Exchange,
    result: Result<AccountSnapshot, AccountManagerError>,
}

#[async_trait]
pub trait AccountManager {
    async fn account_model(&self, exchange: Exchange) -> Result<AccountModel, AccountManagerError>;

    async fn list_partitions(
        &self,
        exchange: Exchange,
    ) -> Result<Vec<AccountPartitionRef>, AccountManagerError>;

    async fn get_balances(
        &self,
        query: BalanceQuery,
    ) -> Result<Vec<BalanceView>, AccountManagerError>;

    async fn get_effective_collateral(
        &self,
        exchange: Exchange,
        quote_asset: Currency,
        partition: Option<AccountPartitionRef>,
    ) -> Result<CollateralSummary, AccountManagerError>;

    async fn get_margin_state(
        &self,
        exchange: Exchange,
        partition: Option<AccountPartitionRef>,
    ) -> Result<MarginStateView, AccountManagerError>;

    async fn reconcile_snapshot(
        &self,
        exchange: Exchange,
    ) -> Result<AccountSnapshot, AccountManagerError>;

    async fn get_snapshot(
        &self,
        exchange: Exchange,
    ) -> Result<AccountSnapshot, AccountManagerError>;
}

#[async_trait]
pub trait AccountSnapshotSource {
    async fn fetch_account_snapshot(
        &self,
        exchange: Exchange,
    ) -> Result<AccountSnapshot, AccountManagerError>;
}

// Blanket impl: Arc<T> satisfies AccountSnapshotSource when T does.
#[async_trait]
impl<T: AccountSnapshotSource + Send + Sync> AccountSnapshotSource for std::sync::Arc<T> {
    async fn fetch_account_snapshot(
        &self,
        exchange: Exchange,
    ) -> Result<AccountSnapshot, AccountManagerError> {
        self.as_ref().fetch_account_snapshot(exchange).await
    }
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountEventKind {
    SnapshotUpdated,
    SnapshotReconciled,
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone)]
pub struct AccountEvent {
    pub exchange: Exchange,
    pub kind: AccountEventKind,
    pub as_of_utc: DateTime<Utc>,
}

enum AccountCommand {
    AccountModel {
        exchange: Exchange,
        reply_to: oneshot::Sender<Result<AccountModel, AccountManagerError>>,
    },
    ListPartitions {
        exchange: Exchange,
        reply_to: oneshot::Sender<Result<Vec<AccountPartitionRef>, AccountManagerError>>,
    },
    GetBalances {
        query: BalanceQuery,
        reply_to: oneshot::Sender<Result<Vec<BalanceView>, AccountManagerError>>,
    },
    GetEffectiveCollateral {
        exchange: Exchange,
        quote_asset: Currency,
        partition: Option<AccountPartitionRef>,
        reply_to: oneshot::Sender<Result<CollateralSummary, AccountManagerError>>,
    },
    GetMarginState {
        exchange: Exchange,
        partition: Option<AccountPartitionRef>,
        reply_to: oneshot::Sender<Result<MarginStateView, AccountManagerError>>,
    },
    ReconcileSnapshot {
        exchange: Exchange,
        reply_to: oneshot::Sender<Result<AccountSnapshot, AccountManagerError>>,
    },
    GetSnapshot {
        exchange: Exchange,
        reply_to: oneshot::Sender<Result<AccountSnapshot, AccountManagerError>>,
    },
}

pub struct AccountManagerActor<S>
where
    S: AccountSnapshotSource + Send + Sync + 'static,
{
    source: Arc<S>,
    rx: mpsc::Receiver<AccountCommand>,
    /// Sender end for internal fetch-completion notifications
    fetch_result_tx: mpsc::Sender<FetchResult>,
    fetch_result_rx: mpsc::Receiver<FetchResult>,
    event_tx: broadcast::Sender<AccountEvent>,
    snapshots: HashMap<Exchange, AccountSnapshot>,
    /// Commands waiting for an in-flight fetch to complete
    pending: HashMap<Exchange, Vec<AccountCommand>>,
    /// Exchanges that currently have a background fetch in-flight
    fetching: HashSet<Exchange>,
    config: AccountManagerConfig,
}

#[derive(Clone)]
pub struct AccountManagerHandle {
    tx: mpsc::Sender<AccountCommand>,
    event_tx: broadcast::Sender<AccountEvent>,
}

impl<S> AccountManagerActor<S>
where
    S: AccountSnapshotSource + Send + Sync + 'static,
{
    /// Spawn the actor and return a handle for sending commands to it.
    pub fn spawn(source: S, config: AccountManagerConfig) -> AccountManagerHandle {
        let (tx, rx) = mpsc::channel(config.mailbox_capacity);
        let (event_tx, _) = broadcast::channel(config.mailbox_capacity.max(16));
        let (fetch_result_tx, fetch_result_rx) = mpsc::channel(64);

        let actor = Self {
            source: Arc::new(source),
            rx,
            fetch_result_tx,
            fetch_result_rx,
            event_tx: event_tx.clone(),
            snapshots: HashMap::new(),
            pending: HashMap::new(),
            fetching: HashSet::new(),
            config,
        };

        tokio::spawn(async move {
            actor.run().await;
        });

        AccountManagerHandle { tx, event_tx }
    }

    /// Main event loop: select on inbound commands and background fetch completions.
    /// Cached reads are served immediately; uncached/stale reads queue until the fetch finishes.
    async fn run(mut self) {
        loop {
            tokio::select! {
                Some(command) = self.rx.recv() => {
                    self.handle_command(command).await;
                }
                Some(result) = self.fetch_result_rx.recv() => {
                    self.handle_fetch_result(result).await;
                }
                else => break,
            }
        }
    }

    // ──────────────────────────────────────────────────────────────────────────
    // Core routing helpers
    // ──────────────────────────────────────────────────────────────────────────

    async fn handle_command(&mut self, command: AccountCommand) {
        let exchange = Self::command_exchange(&command);

        if let Some(exchange) = exchange {
            let is_reconcile = matches!(command, AccountCommand::ReconcileSnapshot { .. });

            if is_reconcile {
                // ReconcileSnapshot always forces a fresh fetch.
                if !self.fetching.contains(&exchange) {
                    self.start_fetch(exchange);
                }
                self.pending.entry(exchange).or_default().push(command);
                return;
            }

            if !self.snapshots.contains_key(&exchange) {
                // No snapshot cached yet — start a fetch and queue the command.
                if !self.fetching.contains(&exchange) {
                    self.start_fetch(exchange);
                }
                self.pending.entry(exchange).or_default().push(command);
                return;
            }

            if self.is_stale(exchange) {
                // Stale snapshot: trigger a background refresh but serve cached data right away.
                warn!(exchange = ?exchange, "Serving stale account snapshot; background refresh triggered");
                if !self.fetching.contains(&exchange) {
                    self.start_fetch(exchange);
                }
                // Fall through — dispatch immediately from stale cache.
            }
        }

        self.dispatch_command(command).await;
    }

    async fn handle_fetch_result(&mut self, result: FetchResult) {
        let exchange = result.exchange;
        self.fetching.remove(&exchange);

        match result.result {
            Ok(snapshot) => {
                let as_of = snapshot.as_of_utc;
                self.snapshots.insert(exchange, snapshot);

                let pending = self.pending.remove(&exchange).unwrap_or_default();

                // Only emit SnapshotUpdated when there is no pending ReconcileSnapshot.
                // ReconcileSnapshot commands will emit SnapshotReconciled from dispatch_command,
                // and SnapshotReconciled already implies SnapshotUpdated semantics.
                let has_reconcile = pending
                    .iter()
                    .any(|cmd| matches!(cmd, AccountCommand::ReconcileSnapshot { .. }));
                if !has_reconcile {
                    let _ = self.event_tx.send(AccountEvent {
                        exchange,
                        kind: AccountEventKind::SnapshotUpdated,
                        as_of_utc: as_of,
                    });
                }

                for cmd in pending {
                    self.dispatch_command(cmd).await;
                }
            }
            Err(e) => {
                tracing::error!(exchange = ?exchange, error = %e, "Background account snapshot fetch failed");
                let error = AccountManagerError::Other {
                    reason: e.to_string(),
                };
                let pending = self.pending.remove(&exchange).unwrap_or_default();
                for cmd in pending {
                    self.reply_with_error(cmd, error.clone());
                }
            }
        }
    }

    fn start_fetch(&mut self, exchange: Exchange) {
        if self.fetching.insert(exchange) {
            let source = Arc::clone(&self.source);
            let tx = self.fetch_result_tx.clone();
            tokio::spawn(async move {
                let result = source.fetch_account_snapshot(exchange).await;
                let _ = tx.send(FetchResult { exchange, result }).await;
            });
        }
    }

    fn is_stale(&self, exchange: Exchange) -> bool {
        match self.snapshots.get(&exchange) {
            Some(snapshot) => (Utc::now() - snapshot.as_of_utc) > self.config.snapshot_max_age,
            None => false,
        }
    }

    /// Dispatch a command against the in-memory cache (snapshot must already be present for
    /// commands that need one, or they must not require a snapshot).
    async fn dispatch_command(&mut self, command: AccountCommand) {
        match command {
            AccountCommand::AccountModel { exchange, reply_to } => {
                let _ = reply_to.send(self.account_model_impl(exchange));
            }
            AccountCommand::ListPartitions { exchange, reply_to } => {
                let _ = reply_to.send(self.list_partitions_impl(exchange));
            }
            AccountCommand::GetBalances { query, reply_to } => {
                let _ = reply_to.send(self.get_balances_impl(query));
            }
            AccountCommand::GetEffectiveCollateral {
                exchange,
                quote_asset,
                partition,
                reply_to,
            } => {
                let _ = reply_to.send(self.get_effective_collateral_impl(
                    exchange,
                    quote_asset,
                    partition,
                ));
            }
            AccountCommand::GetMarginState {
                exchange,
                partition,
                reply_to,
            } => {
                let _ = reply_to.send(self.get_margin_state_impl(exchange, partition));
            }
            AccountCommand::ReconcileSnapshot { exchange, reply_to } => {
                let _ = reply_to.send(self.reconcile_snapshot_impl(exchange));
            }
            AccountCommand::GetSnapshot { exchange, reply_to } => {
                let _ = reply_to.send(self.get_snapshot_impl(exchange));
            }
        }
    }

    fn reply_with_error(&self, command: AccountCommand, error: AccountManagerError) {
        match command {
            AccountCommand::AccountModel { reply_to, .. } => {
                let _ = reply_to.send(Err(error));
            }
            AccountCommand::ListPartitions { reply_to, .. } => {
                let _ = reply_to.send(Err(error));
            }
            AccountCommand::GetBalances { reply_to, .. } => {
                let _ = reply_to.send(Err(error));
            }
            AccountCommand::GetEffectiveCollateral { reply_to, .. } => {
                let _ = reply_to.send(Err(error));
            }
            AccountCommand::GetMarginState { reply_to, .. } => {
                let _ = reply_to.send(Err(error));
            }
            AccountCommand::ReconcileSnapshot { reply_to, .. } => {
                let _ = reply_to.send(Err(error));
            }
            AccountCommand::GetSnapshot { reply_to, .. } => {
                let _ = reply_to.send(Err(error));
            }
        }
    }

    fn command_exchange(command: &AccountCommand) -> Option<Exchange> {
        match command {
            AccountCommand::AccountModel { exchange, .. } => Some(*exchange),
            AccountCommand::ListPartitions { exchange, .. } => Some(*exchange),
            AccountCommand::GetBalances { query, .. } => Some(query.exchange),
            AccountCommand::GetEffectiveCollateral { exchange, .. } => Some(*exchange),
            AccountCommand::GetMarginState { exchange, .. } => Some(*exchange),
            AccountCommand::ReconcileSnapshot { exchange, .. } => Some(*exchange),
            AccountCommand::GetSnapshot { exchange, .. } => Some(*exchange),
        }
    }

    // ──────────────────────────────────────────────────────────────────────────
    // Per-query implementations — all synchronous, read from in-memory cache
    // ──────────────────────────────────────────────────────────────────────────

    fn get_snapshot_cached(
        &self,
        exchange: Exchange,
    ) -> Result<&AccountSnapshot, AccountManagerError> {
        self.snapshots
            .get(&exchange)
            .ok_or_else(|| AccountManagerError::Other {
                reason: format!("no snapshot cached for {:?}", exchange),
            })
    }

    fn account_model_impl(&self, exchange: Exchange) -> Result<AccountModel, AccountManagerError> {
        Ok(self.get_snapshot_cached(exchange)?.model)
    }

    fn list_partitions_impl(
        &self,
        exchange: Exchange,
    ) -> Result<Vec<AccountPartitionRef>, AccountManagerError> {
        Ok(self.get_snapshot_cached(exchange)?.partitions.clone())
    }

    fn get_balances_impl(
        &self,
        query: BalanceQuery,
    ) -> Result<Vec<BalanceView>, AccountManagerError> {
        let snapshot = self.get_snapshot_cached(query.exchange)?;
        let balances = snapshot
            .balances
            .iter()
            .filter(|balance| {
                if let Some(asset) = query.asset {
                    balance.asset == asset
                } else {
                    true
                }
            })
            .filter(|balance| {
                if let Some(partition) = &query.partition {
                    balance.partition.as_ref() == Some(partition)
                } else {
                    true
                }
            })
            .filter(|balance| {
                if query.include_zero_balances {
                    true
                } else {
                    !balance.total.is_zero() || !balance.unrealized_pnl.is_zero()
                }
            })
            .cloned()
            .collect();

        Ok(balances)
    }

    fn get_effective_collateral_impl(
        &self,
        exchange: Exchange,
        quote_asset: Currency,
        partition: Option<AccountPartitionRef>,
    ) -> Result<CollateralSummary, AccountManagerError> {
        let snapshot = self.get_snapshot_cached(exchange)?;

        if let Some(ref p) = partition {
            let exists = snapshot.partitions.iter().any(|candidate| candidate == p);
            if !exists {
                return Err(AccountManagerError::PartitionNotFound {
                    exchange,
                    partition: format!("{:?}/{:?}", p.kind, p.account_id),
                });
            }
        }

        let relevant: Vec<&BalanceView> = snapshot
            .balances
            .iter()
            .filter(|balance| balance.asset == quote_asset)
            .filter(|balance| match snapshot.model {
                // Unified + no partition: all balances contribute to total collateral
                AccountModel::Unified => match partition.as_ref() {
                    None => true,
                    Some(target) => match balance.collateral_scope {
                        CollateralScope::SharedAcrossPartitions => true,
                        CollateralScope::PartitionOnly => {
                            balance.partition.as_ref() == Some(target)
                        }
                    },
                },
                AccountModel::Segmented => match partition.as_ref() {
                    Some(target) => balance.partition.as_ref() == Some(target),
                    None => false,
                },
            })
            .collect();

        let effective_collateral: Decimal = relevant.iter().map(|b| b.net_equity()).sum();
        let total_balance: Decimal = relevant.iter().map(|b| b.total).sum();
        let total_borrowed: Decimal = relevant.iter().map(|b| b.borrowed).sum();
        let total_unrealized_pnl: Decimal = relevant.iter().map(|b| b.unrealized_pnl).sum();

        Ok(CollateralSummary {
            exchange,
            model: snapshot.model,
            quote_asset,
            partition,
            effective_collateral,
            total_balance,
            total_borrowed,
            total_unrealized_pnl,
            as_of_utc: snapshot.as_of_utc,
        })
    }

    fn get_margin_state_impl(
        &self,
        exchange: Exchange,
        partition: Option<AccountPartitionRef>,
    ) -> Result<MarginStateView, AccountManagerError> {
        let snapshot = self.get_snapshot_cached(exchange)?;

        if let Some(ref p) = partition {
            let exists = snapshot.partitions.iter().any(|candidate| candidate == p);
            if !exists {
                return Err(AccountManagerError::PartitionNotFound {
                    exchange,
                    partition: format!("{:?}/{:?}", p.kind, p.account_id),
                });
            }
        }

        let mode = match snapshot.model {
            AccountModel::Unified => MarginMode::Portfolio,
            AccountModel::Segmented => MarginMode::Cross,
        };

        Ok(MarginStateView {
            exchange,
            partition,
            mode,
            margin_ratio: None,
            initial_margin_ratio: None,
            maintenance_margin_ratio: None,
            as_of_utc: snapshot.as_of_utc,
        })
    }

    /// Process a reconcile command: snapshot is already fresh in cache (just fetched).
    /// Emits `SnapshotReconciled` and returns the snapshot.
    fn reconcile_snapshot_impl(
        &self,
        exchange: Exchange,
    ) -> Result<AccountSnapshot, AccountManagerError> {
        let snapshot = self.get_snapshot_cached(exchange)?.clone();

        tracing::info!(
            exchange = ?exchange,
            model = ?snapshot.model,
            partitions = snapshot.partitions.len(),
            balances = snapshot.balances.len(),
            as_of = %snapshot.as_of_utc,
            "Account snapshot reconciled:{}",
            BalancesDisplay(&snapshot.balances)
        );

        let _ = self.event_tx.send(AccountEvent {
            exchange,
            kind: AccountEventKind::SnapshotReconciled,
            as_of_utc: snapshot.as_of_utc,
        });
        Ok(snapshot)
    }
    fn get_snapshot_impl(
        &self,
        exchange: Exchange,
    ) -> Result<AccountSnapshot, AccountManagerError> {
        let snapshot = self.get_snapshot_cached(exchange)?;
        Ok(snapshot.clone())
    }
}

impl AccountManagerHandle {
    pub fn subscribe_events(&self) -> broadcast::Receiver<AccountEvent> {
        self.event_tx.subscribe()
    }

    async fn request<T>(
        &self,
        build: impl FnOnce(oneshot::Sender<Result<T, AccountManagerError>>) -> AccountCommand,
    ) -> Result<T, AccountManagerError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(build(reply_tx))
            .await
            .map_err(|_| AccountManagerError::ActorMailboxClosed)?;

        reply_rx
            .await
            .map_err(|_| AccountManagerError::ActorResponseDropped)?
    }
}

#[async_trait]
impl AccountManager for AccountManagerHandle {
    async fn account_model(&self, exchange: Exchange) -> Result<AccountModel, AccountManagerError> {
        self.request(|reply_to| AccountCommand::AccountModel { exchange, reply_to })
            .await
    }

    async fn list_partitions(
        &self,
        exchange: Exchange,
    ) -> Result<Vec<AccountPartitionRef>, AccountManagerError> {
        self.request(|reply_to| AccountCommand::ListPartitions { exchange, reply_to })
            .await
    }

    async fn get_balances(
        &self,
        query: BalanceQuery,
    ) -> Result<Vec<BalanceView>, AccountManagerError> {
        self.request(|reply_to| AccountCommand::GetBalances { query, reply_to })
            .await
    }

    async fn get_effective_collateral(
        &self,
        exchange: Exchange,
        quote_asset: Currency,
        partition: Option<AccountPartitionRef>,
    ) -> Result<CollateralSummary, AccountManagerError> {
        self.request(|reply_to| AccountCommand::GetEffectiveCollateral {
            exchange,
            quote_asset,
            partition,
            reply_to,
        })
        .await
    }

    async fn get_margin_state(
        &self,
        exchange: Exchange,
        partition: Option<AccountPartitionRef>,
    ) -> Result<MarginStateView, AccountManagerError> {
        self.request(|reply_to| AccountCommand::GetMarginState {
            exchange,
            partition,
            reply_to,
        })
        .await
    }

    async fn reconcile_snapshot(
        &self,
        exchange: Exchange,
    ) -> Result<AccountSnapshot, AccountManagerError> {
        self.request(|reply_to| AccountCommand::ReconcileSnapshot { exchange, reply_to })
            .await
    }

    async fn get_snapshot(
        &self,
        exchange: Exchange,
    ) -> Result<AccountSnapshot, AccountManagerError> {
        self.request(|reply_to| AccountCommand::GetSnapshot { exchange, reply_to })
            .await
    }
}

pub fn encode_account_event_rkyv(event: &AccountEvent) -> Result<Vec<u8>, AccountManagerError> {
    let bytes = rkyv::to_bytes::<_, 64>(event).map_err(|err| AccountManagerError::Other {
        reason: format!("rkyv encode error: {err}"),
    })?;

    Ok(bytes.into_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::types::{BalanceView, CollateralScope, PartitionKind};
    use rust_decimal_macros::dec;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct MockSnapshotSource {
        snapshots: HashMap<Exchange, AccountSnapshot>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl AccountSnapshotSource for MockSnapshotSource {
        async fn fetch_account_snapshot(
            &self,
            exchange: Exchange,
        ) -> Result<AccountSnapshot, AccountManagerError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.snapshots
                .get(&exchange)
                .cloned()
                .ok_or_else(|| AccountManagerError::Other {
                    reason: format!("no snapshot for {:?}", exchange),
                })
        }
    }

    fn partition(exchange: Exchange, kind: PartitionKind, id: &str) -> AccountPartitionRef {
        AccountPartitionRef {
            exchange,
            kind,
            account_id: Some(id.to_string()),
        }
    }

    fn unified_snapshot(exchange: Exchange) -> AccountSnapshot {
        let swap = partition(exchange, PartitionKind::Swap, "swap");
        AccountSnapshot {
            exchange,
            model: AccountModel::Unified,
            partitions: vec![swap.clone()],
            balances: vec![
                BalanceView {
                    exchange,
                    partition: None,
                    asset: Currency::USDT,
                    total: dec!(1000),
                    free: dec!(900),
                    locked: dec!(100),
                    borrowed: dec!(0),
                    unrealized_pnl: dec!(0),
                    collateral_scope: CollateralScope::SharedAcrossPartitions,
                    as_of_utc: Utc::now(),
                },
                BalanceView {
                    exchange,
                    partition: Some(swap),
                    asset: Currency::USDT,
                    total: dec!(200),
                    free: dec!(200),
                    locked: dec!(0),
                    borrowed: dec!(0),
                    unrealized_pnl: dec!(10),
                    collateral_scope: CollateralScope::PartitionOnly,
                    as_of_utc: Utc::now(),
                },
            ],
            as_of_utc: Utc::now(),
        }
    }

    #[tokio::test]
    async fn actor_returns_cached_model_and_only_fetches_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut snapshots = HashMap::new();
        snapshots.insert(Exchange::Bybit, unified_snapshot(Exchange::Bybit));

        let source = MockSnapshotSource {
            snapshots,
            calls: calls.clone(),
        };

        let handle = AccountManagerActor::spawn(
            source,
            AccountManagerConfig {
                mailbox_capacity: 32,
                ..Default::default()
            },
        );
        let model_a = handle.account_model(Exchange::Bybit).await.unwrap();
        let model_b = handle.account_model(Exchange::Bybit).await.unwrap();

        assert_eq!(model_a, AccountModel::Unified);
        assert_eq!(model_b, AccountModel::Unified);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn actor_computes_unified_effective_collateral() {
        let mut snapshots = HashMap::new();
        snapshots.insert(Exchange::Bybit, unified_snapshot(Exchange::Bybit));

        let source = MockSnapshotSource {
            snapshots,
            calls: Arc::new(AtomicUsize::new(0)),
        };

        let handle = AccountManagerActor::spawn(
            source,
            AccountManagerConfig {
                mailbox_capacity: 32,
                ..Default::default()
            },
        );
        let summary = handle
            .get_effective_collateral(Exchange::Bybit, Currency::USDT, None)
            .await
            .unwrap();

        assert_eq!(summary.model, AccountModel::Unified);
        assert_eq!(summary.effective_collateral, dec!(1210));
    }

    #[tokio::test]
    async fn actor_emits_reconcile_event() {
        let mut snapshots = HashMap::new();
        snapshots.insert(Exchange::Binance, unified_snapshot(Exchange::Binance));

        let source = MockSnapshotSource {
            snapshots,
            calls: Arc::new(AtomicUsize::new(0)),
        };

        let handle = AccountManagerActor::spawn(
            source,
            AccountManagerConfig {
                mailbox_capacity: 32,
                ..Default::default()
            },
        );
        let mut rx = handle.subscribe_events();

        let _ = handle.reconcile_snapshot(Exchange::Binance).await.unwrap();
        let event = rx.recv().await.unwrap();

        assert_eq!(event.exchange, Exchange::Binance);
        assert_eq!(event.kind, AccountEventKind::SnapshotReconciled);
    }
}
