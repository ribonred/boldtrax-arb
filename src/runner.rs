//! Exchange runner: wires up instruments, account polling, funding-rate
//! polling, and the live order-book price feed into one cohesive task set.
//!
//! `ExchangeRunner` is intentionally thin right now — it owns the lifecycle of
//! all sub-tasks and will grow as more strategies and risk components are added.

use std::sync::Arc;
use std::time::Duration;

use boldtrax_core::manager::account::{
    AccountManager, AccountManagerActor, AccountManagerConfig, AccountManagerHandle,
    AccountSnapshotSource,
};
use boldtrax_core::manager::order::{OrderManagerActor, OrderManagerConfig, OrderManagerHandle};
use boldtrax_core::manager::position::{
    PositionManagerActor, PositionManagerConfig, PositionManagerHandle,
};
use boldtrax_core::manager::price::{
    PriceManager, PriceManagerActor, PriceManagerConfig, PriceManagerHandle,
};
use boldtrax_core::registry::InstrumentRegistry;
use boldtrax_core::traits::{
    FundingRateMarketData, MarketDataProvider, OrderBookFeeder, OrderExecutionProvider,
};
use boldtrax_core::types::{Exchange, ExecutionMode, InstrumentKey};
use thiserror::Error;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use boldtrax_core::zmq::protocol::ZmqEvent;
use boldtrax_core::zmq::server::{ZmqServer, ZmqServerConfig};

#[derive(Debug, Error)]
pub enum RunnerError {
    #[error("instrument load failed: {0}")]
    InstrumentLoad(String),

    #[error("task join error: {0}")]
    TaskJoin(String),
}

/// Top-level configuration for a single `ExchangeRunner`.
#[derive(Debug, Clone)]
pub struct ExchangeRunnerConfig {
    /// The target exchange (used for account reconciliation calls).
    pub exchange: Exchange,

    /// The execution mode (Live or Paper).
    pub execution_mode: ExecutionMode,

    /// How often to force an account snapshot reconciliation.
    pub account_reconcile_interval: Duration,

    /// How often to poll funding-rate snapshots.
    pub funding_rate_poll_interval: Duration,

    /// Instruments to track for price feeds and funding-rate polling.
    /// Must contain swap keys for funding rates; can mix spot + swap for prices.
    pub tracked_keys: Vec<InstrumentKey>,

    /// Forwarded to the internal `AccountManagerActor`.
    pub account_manager_config: AccountManagerConfig,

    /// Forwarded to the internal `PriceManagerActor`.
    pub price_manager_config: PriceManagerConfig,

    /// Forwarded to the internal `OrderManagerActor`.
    pub order_manager_config: OrderManagerConfig,

    /// Forwarded to the internal `PositionManagerActor`.
    pub position_manager_config: PositionManagerConfig,

    /// Configuration for the ZMQ server.
    pub zmq_config: ZmqServerConfig,
}

impl Default for ExchangeRunnerConfig {
    fn default() -> Self {
        Self {
            exchange: Exchange::Binance,
            execution_mode: ExecutionMode::Paper,
            account_reconcile_interval: Duration::from_secs(30),
            funding_rate_poll_interval: Duration::from_secs(10),
            tracked_keys: vec![],
            account_manager_config: AccountManagerConfig::default(),
            price_manager_config: PriceManagerConfig::default(),
            order_manager_config: OrderManagerConfig::default(),
            position_manager_config: PositionManagerConfig::default(),
            zmq_config: ZmqServerConfig {
                exchange: Exchange::Binance,
                redis_url: "redis://127.0.0.1:6379".to_string(),
                ttl_secs: 30,
            },
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Runner
// ──────────────────────────────────────────────────────────────────────────────

/// Orchestrates all exchange-facing tasks for a single exchange client.
///
/// Phase 1 (current):
/// - Load instruments into the shared registry at startup.
/// - Spawn an `AccountManagerActor` and periodically reconcile the snapshot.
/// - Poll funding-rate snapshots for every tracked swap instrument.
/// - Spawn a `PriceManagerActor` that streams 5-level order-books via WebSocket.
///
/// Future phases will add strategy runners, risk monitors, and order management.
pub struct ExchangeRunner<C> {
    client: Arc<C>,
    registry: InstrumentRegistry,
    config: ExchangeRunnerConfig,
}

impl<C> ExchangeRunner<C>
where
    C: MarketDataProvider
        + FundingRateMarketData
        + AccountSnapshotSource
        + OrderBookFeeder
        + OrderExecutionProvider
        + Send
        + Sync
        + 'static,
{
    /// Create a new runner with `client` wrapped in an `Arc` internally so it
    /// can be shared across all spawned tasks without cloning.
    pub fn new(client: C, config: ExchangeRunnerConfig) -> Self {
        Self {
            client: Arc::new(client),
            registry: InstrumentRegistry::new(),
            config,
        }
    }

    /// With a pre-built `Arc<C>` (e.g. when the caller needs the client handle
    /// independently of the runner).
    #[allow(dead_code)]
    pub fn with_arc(client: Arc<C>, config: ExchangeRunnerConfig) -> Self {
        Self {
            client,
            registry: InstrumentRegistry::new(),
            config,
        }
    }

    /// Run all tasks until Ctrl-C is received or a critical task fails.
    pub async fn run(self) -> Result<(), RunnerError> {
        info!(
            exchange = ?self.config.exchange,
            mode = ?self.config.execution_mode,
            "Starting ExchangeRunner"
        );
        info!(exchange = ?self.config.exchange, "Loading instruments");
        self.client
            .load_instruments(&self.registry)
            .await
            .map_err(|e| RunnerError::InstrumentLoad(e.to_string()))?;
        info!(
            exchange = ?self.config.exchange,
            total = self.registry.len(),
            "Instruments loaded"
        );

        // ── 2. Spawn account manager actor
        let account_handle: AccountManagerHandle = AccountManagerActor::spawn(
            Arc::clone(&self.client),
            self.config.account_manager_config.clone(),
        );
        info!(exchange = ?self.config.exchange, "AccountManagerActor spawned");

        // ── 3. Spawn price manager actor
        let price_handle: PriceManagerHandle = PriceManagerActor::spawn(
            Arc::clone(&self.client),
            self.config.tracked_keys.clone(),
            self.config.price_manager_config.clone(),
        );
        info!(exchange = ?self.config.exchange, keys = self.config.tracked_keys.len(), "PriceManagerActor spawned");

        // ── 4. Spawn order manager actor
        let order_handle: OrderManagerHandle = OrderManagerActor::spawn(
            self.config.exchange,
            Arc::clone(&self.client),
            self.registry.clone(),
            self.config.order_manager_config.clone(),
        );
        info!(exchange = ?self.config.exchange, "OrderManagerActor spawned");

        // ── 5. Spawn position manager actor
        let position_handle: PositionManagerHandle = PositionManagerActor::spawn(
            order_handle.subscribe_events(),
            self.config.position_manager_config.clone(),
        );
        info!(exchange = ?self.config.exchange, "PositionManagerActor spawned");

        let (zmq_tx, zmq_rx) = broadcast::channel(1024);

        let zmq_server = ZmqServer::new(
            self.config.zmq_config.clone(),
            Some(order_handle.clone()),
            Some(position_handle.clone()),
            Some(account_handle.clone()),
            self.registry.clone(),
            zmq_rx,
        );

        let zmq_task = tokio::spawn(async move {
            if let Err(e) = zmq_server.run().await {
                error!(error = %e, "ZmqServer failed");
            }
        });

        // Forward PriceManager updates to ZMQ
        let mut price_rx = price_handle.subscribe_updates();
        let zmq_tx_price = zmq_tx.clone();
        tokio::spawn(async move {
            while let Ok(update) = price_rx.recv().await {
                let _ = zmq_tx_price.send(ZmqEvent::OrderBookUpdate(update));
            }
        });

        // Forward OrderManager updates to ZMQ
        let mut order_rx = order_handle.subscribe_events();
        let zmq_tx_order = zmq_tx.clone();
        tokio::spawn(async move {
            while let Ok(event) = order_rx.recv().await {
                let _ = zmq_tx_order.send(ZmqEvent::OrderUpdate(event));
            }
        });

        // Forward PositionManager updates to ZMQ
        let mut position_rx = position_handle.subscribe_events();
        let zmq_tx_position = zmq_tx.clone();
        tokio::spawn(async move {
            while let Ok(event) = position_rx.recv().await {
                let _ = zmq_tx_position.send(ZmqEvent::PositionUpdate(event));
            }
        });

        // ── 4. Account reconcile loop
        let account_task: JoinHandle<()> = {
            let handle = account_handle.clone();
            let exchange = self.config.exchange;
            let interval = self.config.account_reconcile_interval;
            tokio::spawn(async move {
                info!(exchange = ?exchange, interval_secs = interval.as_secs(), "Account reconcile loop started");
                loop {
                    tokio::time::sleep(interval).await;
                    match handle.reconcile_snapshot(exchange).await {
                        Ok(snap) => {
                            info!(
                                exchange = ?exchange,
                                partitions = snap.partitions.len(),
                                balances = snap.balances.len(),
                                as_of = %snap.as_of_utc,
                                "Account snapshot reconciled"
                            );
                        }
                        Err(e) => {
                            error!(exchange = ?exchange, error = %e, "Account reconcile failed");
                        }
                    }
                }
            })
        };

        // ── 5. Funding rate poll loop
        let funding_task: JoinHandle<()> = {
            let client = Arc::clone(&self.client);
            let registry = self.registry.clone();
            let keys = self.config.tracked_keys.clone();
            let interval = self.config.funding_rate_poll_interval;
            let exchange = self.config.exchange;
            let zmq_tx_funding = zmq_tx.clone();
            tokio::spawn(async move {
                let swap_keys: Vec<InstrumentKey> = keys
                    .into_iter()
                    .filter(|k| k.instrument_type == boldtrax_core::types::InstrumentType::Swap)
                    .collect();

                if swap_keys.is_empty() {
                    warn!(exchange = ?exchange, "No swap instruments tracked; funding rate polling skipped");
                    return;
                }

                info!(
                    exchange = ?exchange,
                    instruments = swap_keys.len(),
                    interval_secs = interval.as_secs(),
                    "Funding rate poll loop started"
                );
                loop {
                    for key in &swap_keys {
                        match client.funding_rate_snapshot(*key, &registry).await {
                            Ok(snap) => {
                                let _ = zmq_tx_funding.send(ZmqEvent::FundingRate(snap.clone()));
                                info!(
                                    key = ?key,
                                    rate = %snap.funding_rate,
                                    mark_price = %snap.mark_price,
                                    next_funding = %snap.next_funding_time_utc,
                                    "Funding rate snapshot"
                                );
                            }
                            Err(e) => {
                                error!(key = ?key, error = %e, "Funding rate fetch failed");
                            }
                        }
                    }
                    tokio::time::sleep(interval).await;
                }
            })
        };

        info!(exchange = ?self.config.exchange, "ExchangeRunner running — press Ctrl-C to stop");

        let _ = price_handle; // keep alive (actor runs autonomously)

        tokio::select! {
            res = account_task => {
                if let Err(e) = res {
                    error!(error = %e, "Account task panicked");
                    return Err(RunnerError::TaskJoin(e.to_string()));
                }
            }
            res = funding_task => {
                if let Err(e) = res {
                    error!(error = %e, "Funding rate task panicked");
                    return Err(RunnerError::TaskJoin(e.to_string()));
                }
            }
            res = zmq_task => {
                if let Err(e) = res {
                    error!(error = %e, "ZMQ task panicked");
                    return Err(RunnerError::TaskJoin(e.to_string()));
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl-C received — shutting down ExchangeRunner");
            }
        }

        Ok(())
    }
}
