//! Exchange runner: wires up instruments, account polling, funding-rate
//! polling, and the live order-book price feed into one cohesive task set.
//!
//! `ExchangeRunner` is intentionally thin right now — it owns the lifecycle of
//! all sub-tasks and will grow as more strategies and risk components are added.

use std::sync::Arc;
use std::time::Duration;

use boldtrax_core::config::types::AppConfig;
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
    FundingRateMarketData, LeverageProvider, MarketDataProvider, OrderBookFeeder,
    OrderExecutionProvider, PositionProvider,
};
use boldtrax_core::types::{Exchange, ExecutionMode, InstrumentKey};
use thiserror::Error;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use boldtrax_core::zmq::discovery::DiscoveryClient;
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

impl ExchangeRunnerConfig {
    pub fn from_app_config(
        app_config: &AppConfig,
        exchange: Exchange,
        tracked_keys: Vec<InstrumentKey>,
    ) -> Self {
        let rc = &app_config.runner;
        let mgrs = &rc.managers;

        let account_manager_config = AccountManagerConfig {
            mailbox_capacity: mgrs.account.mailbox_capacity,
            snapshot_max_age: chrono::Duration::seconds(mgrs.account.snapshot_max_age_secs as i64),
        };

        let price_manager_config = PriceManagerConfig {
            mailbox_capacity: mgrs.price.mailbox_capacity,
            event_broadcast_capacity: mgrs.price.broadcast_capacity,
            ws_channel_capacity: mgrs.price.ws_capacity,
            max_capacity: mgrs.price.max_cache_entries,
            time_to_live: Duration::from_secs(mgrs.price.ttl_secs),
        };

        let order_manager_config = OrderManagerConfig {
            mailbox_capacity: mgrs.order.mailbox_capacity,
            event_broadcast_capacity: mgrs.order.broadcast_capacity,
            ws_channel_capacity: mgrs.order.ws_capacity,
            reconcile_interval: Duration::from_secs(mgrs.order.reconcile_interval_secs),
            fat_finger: app_config.risk.fat_finger.clone(),
        };

        let position_manager_config = PositionManagerConfig {
            mailbox_capacity: mgrs.position.mailbox_capacity,
            event_broadcast_capacity: mgrs.position.broadcast_capacity,
            reconcile_interval: Duration::from_secs(mgrs.position.reconcile_interval_secs),
        };

        let zmq_config = ZmqServerConfig {
            exchange,
            redis_url: app_config.redis_url.clone(),
            ttl_secs: rc.zmq_ttl_secs,
        };

        Self {
            exchange,
            execution_mode: app_config.execution_mode,
            account_reconcile_interval: rc.account_reconcile_interval(),
            funding_rate_poll_interval: rc.funding_rate_poll_interval(),
            tracked_keys,
            account_manager_config,
            price_manager_config,
            order_manager_config,
            position_manager_config,
            zmq_config,
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
        + PositionProvider
        + LeverageProvider
        + Send
        + Sync
        + 'static,
{
    /// Create a new runner with `client` wrapped in an `Arc` internally so it
    /// can be shared across all spawned tasks without cloning.
    pub fn new(client: C, config: ExchangeRunnerConfig, registry: InstrumentRegistry) -> Self {
        Self {
            client: Arc::new(client),
            registry,
            config,
        }
    }

    /// With a pre-built `Arc<C>` (e.g. when the caller needs the client handle
    /// independently of the runner).
    #[allow(dead_code)]
    pub fn with_arc(
        client: Arc<C>,
        config: ExchangeRunnerConfig,
        registry: InstrumentRegistry,
    ) -> Self {
        Self {
            client,
            registry,
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

        // Clear stale ZMQ discovery keys from a previous run so that
        // poll_until_ready won't find dead endpoints.
        if let Ok(discovery) = DiscoveryClient::new(&self.config.zmq_config.redis_url) {
            if let Err(e) = discovery.deregister_all(self.config.exchange).await {
                warn!(error = %e, "Failed to deregister stale ZMQ endpoints — continuing");
            } else {
                info!(exchange = ?self.config.exchange, "Cleared stale ZMQ discovery keys");
            }
        }

        info!(exchange = ?self.config.exchange, "Loading instruments");
        self.client
            .load_instruments()
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
            Some(price_handle.subscribe_updates()),
        );
        info!(exchange = ?self.config.exchange, "OrderManagerActor spawned");

        // ── 5. Spawn position manager actor
        let position_handle: PositionManagerHandle = PositionManagerActor::spawn(
            Arc::clone(&self.client),
            self.config.position_manager_config.clone(),
        );
        info!(exchange = ?self.config.exchange, "PositionManagerActor spawned");

        let (zmq_tx, zmq_rx) = broadcast::channel(1024);

        let zmq_server = ZmqServer::builder(
            self.config.zmq_config.clone(),
            self.registry.clone(),
            zmq_rx,
        )
        .order_handle(order_handle.clone())
        .position_handle(position_handle.clone())
        .account_handle(account_handle.clone())
        .price_handle(price_handle.clone())
        .leverage_provider(Arc::clone(&self.client) as Arc<dyn LeverageProvider>)
        .funding_rate_provider(
            Arc::clone(&self.client) as Arc<dyn FundingRateMarketData + Send + Sync>
        )
        .build();

        let zmq_task = tokio::spawn(async move {
            if let Err(e) = zmq_server.run().await {
                error!(error = ?e, "ZmqServer failed");
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
                    if let Err(e) = handle.reconcile_snapshot(exchange).await {
                        error!(exchange = ?exchange, error = %e, "Account reconcile failed");
                    }
                    tokio::time::sleep(interval).await;
                }
            })
        };

        // ── 5. Funding rate poll loop
        let funding_task: JoinHandle<()> = {
            let client = Arc::clone(&self.client);
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
                        match client.funding_rate_snapshot(*key).await {
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
