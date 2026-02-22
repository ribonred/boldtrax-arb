//! Multi-exchange order-book price manager.
//!
//! `PriceManagerActor<F>` owns a single `OrderBookFeeder` and manages live
//! order-book state for a set of instruments. A WebSocket task forwards
//! `OrderBookUpdate` messages to the actor via an mpsc channel; the actor
//! merges them into its in-memory cache and publishes them on a broadcast
//! channel for downstream consumers.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use moka::sync::Cache;
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, error, info, warn};

use crate::traits::{OrderBookFeeder, PriceError};
use crate::types::{InstrumentKey, OrderBookSnapshot, OrderBookUpdate};

#[derive(Debug, Error)]
pub enum PriceManagerError {
    #[error("instrument not found: {key:?}")]
    InstrumentNotFound { key: InstrumentKey },

    #[error("snapshot unavailable (no data received yet) for: {key:?}")]
    SnapshotUnavailable { key: InstrumentKey },

    #[error("price manager actor mailbox is closed")]
    ActorMailboxClosed,

    #[error("price manager actor response channel dropped")]
    ActorResponseDropped,

    #[error("price feeder error: {0}")]
    FeederError(#[from] PriceError),
}

/// Configuration for `PriceManagerActor`.
#[derive(Debug, Clone)]
pub struct PriceManagerConfig {
    /// Size of the actor's command mailbox.
    pub mailbox_capacity: usize,
    /// Capacity of the broadcast channel used to publish `OrderBookUpdate`s.
    pub event_broadcast_capacity: usize,
    /// Capacity of the internal WebSocket update channel.
    pub ws_channel_capacity: usize,
    /// Maximum number of snapshots to keep in the cache.
    pub max_capacity: u64,
    /// Time-to-live for cached snapshots.
    pub time_to_live: std::time::Duration,
}

impl Default for PriceManagerConfig {
    fn default() -> Self {
        Self {
            mailbox_capacity: 64,
            event_broadcast_capacity: 256,
            ws_channel_capacity: 256,
            max_capacity: 10_000,
            time_to_live: std::time::Duration::from_secs(60),
        }
    }
}

/// High-level interface exposed to the rest of the application.
/// `PriceManagerHandle` implements this.
#[async_trait::async_trait]
pub trait PriceManager: Send + Sync {
    /// Return the latest cached order-book snapshot for `key`, or an error if
    /// no data has been received yet.
    async fn get_snapshot(
        &self,
        key: InstrumentKey,
    ) -> Result<OrderBookSnapshot, PriceManagerError>;

    /// Trigger an immediate REST snapshot fetch for `key`.  Useful for
    /// bootstrapping before the WS feed delivers its first update.
    async fn refresh_snapshot(
        &self,
        key: InstrumentKey,
    ) -> Result<OrderBookSnapshot, PriceManagerError>;

    /// Subscribe to a stream of live `OrderBookUpdate` events.
    fn subscribe_updates(&self) -> broadcast::Receiver<OrderBookUpdate>;
}

enum PriceCommand {
    GetSnapshot {
        key: InstrumentKey,
        reply_to: oneshot::Sender<Result<OrderBookSnapshot, PriceManagerError>>,
    },
    RefreshSnapshot {
        key: InstrumentKey,
        reply_to: oneshot::Sender<Result<OrderBookSnapshot, PriceManagerError>>,
    },
}

/// Internal result from a background REST fetch.
struct RestFetchResult {
    key: InstrumentKey,
    result: Result<OrderBookSnapshot, PriceManagerError>,
}

// ──────────────────────────────────────────────────────────────────────────────
// Actor
// ──────────────────────────────────────────────────────────────────────────────

/// Single actor that owns all order-book state.
///
/// It selects concurrently on three sources:
/// 1. **Commands** from external callers (`PriceManagerHandle`).
/// 2. **WebSocket updates** forwarded by the exchange WS task.
/// 3. **REST fetch results** from background snapshot fetches.
pub struct PriceManagerActor<F>
where
    F: OrderBookFeeder,
{
    feeder: Arc<F>,
    rx: mpsc::Receiver<PriceCommand>,
    ws_rx: mpsc::Receiver<OrderBookUpdate>,
    rest_result_tx: mpsc::Sender<RestFetchResult>,
    rest_result_rx: mpsc::Receiver<RestFetchResult>,
    event_tx: broadcast::Sender<OrderBookUpdate>,
    /// Latest order-book per instrument.
    cache: Cache<InstrumentKey, OrderBookSnapshot>,
    /// Pending replies waiting for a REST fetch to complete.
    pending:
        HashMap<InstrumentKey, Vec<oneshot::Sender<Result<OrderBookSnapshot, PriceManagerError>>>>,
    /// Keys that have an in-flight REST fetch.
    fetching: HashSet<InstrumentKey>,
}

// ──────────────────────────────────────────────────────────────────────────────
// Handle
// ──────────────────────────────────────────────────────────────────────────────

/// Cheap-to-clone handle that external code uses to interact with the actor.
#[derive(Clone)]
pub struct PriceManagerHandle {
    tx: mpsc::Sender<PriceCommand>,
    event_tx: broadcast::Sender<OrderBookUpdate>,
}

impl<F> PriceManagerActor<F>
where
    F: OrderBookFeeder + Send + Sync + 'static,
{
    /// Spawn the actor, start the WebSocket feed for the given `keys`, and
    /// return a `PriceManagerHandle`.
    pub fn spawn(
        feeder: F,
        keys: Vec<InstrumentKey>,
        config: PriceManagerConfig,
    ) -> PriceManagerHandle {
        let (tx, rx) = mpsc::channel(config.mailbox_capacity);
        let (event_tx, _) = broadcast::channel(config.event_broadcast_capacity);
        let (ws_tx, ws_rx) = mpsc::channel(config.ws_channel_capacity);
        let (rest_result_tx, rest_result_rx) = mpsc::channel(64);

        let feeder = Arc::new(feeder);

        // Kick off the WebSocket feed task.
        {
            let feeder_ws = Arc::clone(&feeder);
            let ws_tx_clone = ws_tx.clone();
            let key_count = keys.len();
            tokio::spawn(async move {
                info!(instruments = key_count, "PriceManager WS feed task started");
                loop {
                    match feeder_ws
                        .stream_order_books(keys.clone(), ws_tx_clone.clone())
                        .await
                    {
                        Ok(()) => {
                            warn!("WS order-book stream ended; reconnecting");
                        }
                        Err(e) => {
                            error!(error = %e, "WS order-book stream error; reconnecting in 5 s");
                            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                        }
                    }
                }
            });
        }

        let cache = Cache::builder()
            .max_capacity(config.max_capacity)
            .time_to_live(config.time_to_live)
            .build();

        let actor = Self {
            feeder,
            rx,
            ws_rx,
            rest_result_tx,
            rest_result_rx,
            event_tx: event_tx.clone(),
            cache,
            pending: HashMap::new(),
            fetching: HashSet::new(),
        };

        tokio::spawn(async move {
            actor.run().await;
        });

        info!(
            mailbox = config.mailbox_capacity,
            "PriceManagerActor spawned"
        );
        PriceManagerHandle { tx, event_tx }
    }

    /// Main select loop.
    async fn run(mut self) {
        debug!("PriceManagerActor event loop started");
        loop {
            tokio::select! {
                Some(cmd) = self.rx.recv() => {
                    self.handle_command(cmd).await;
                }
                Some(update) = self.ws_rx.recv() => {
                    self.handle_ws_update(update);
                }
                Some(result) = self.rest_result_rx.recv() => {
                    self.handle_rest_result(result);
                }
                else => {
                    info!("PriceManagerActor shutting down (all channels closed)");
                    break;
                }
            }
        }
    }

    async fn handle_command(&mut self, cmd: PriceCommand) {
        match cmd {
            PriceCommand::GetSnapshot { key, reply_to } => {
                if let Some(snapshot) = self.cache.get(&key) {
                    debug!(key = ?key, bid = ?snapshot.best_bid, ask = ?snapshot.best_ask, "GetSnapshot: cache hit");
                    let _ = reply_to.send(Ok(snapshot.clone()));
                } else {
                    debug!(key = ?key, "GetSnapshot: cache miss — queuing REST fetch");
                    // No cached snapshot — fetch via REST and queue the reply.
                    self.pending.entry(key).or_default().push(reply_to);
                    self.maybe_start_rest_fetch(key);
                }
            }
            PriceCommand::RefreshSnapshot { key, reply_to } => {
                debug!(key = ?key, "RefreshSnapshot: forcing REST fetch");
                // Always trigger a fresh REST fetch; queue the reply.
                self.pending.entry(key).or_default().push(reply_to);
                self.start_rest_fetch(key);
            }
        }
    }

    fn handle_ws_update(&mut self, update: OrderBookUpdate) {
        let key = update.snapshot.key;
        let is_first = !self.cache.contains_key(&key);
        self.cache.insert(key, update.snapshot.clone());

        if is_first {
            info!(
                key = ?key,
                best_bid = ?update.snapshot.best_bid,
                best_ask = ?update.snapshot.best_ask,
                "First WS order-book snapshot received"
            );
        } else {
            debug!(
                key = ?key,
                best_bid = ?update.snapshot.best_bid,
                best_ask = ?update.snapshot.best_ask,
                mid = ?update.snapshot.mid,
                "WS order-book update cached"
            );
        }

        // If there were callers waiting for the first snapshot, deliver now.
        if let Some(waiters) = self.pending.remove(&key) {
            debug!(key = ?key, waiters = waiters.len(), "Unblocking pending callers from WS update");
            for tx in waiters {
                let _ = tx.send(Ok(update.snapshot.clone()));
            }
        }

        // Broadcast to external subscribers (ignore "no receivers" case).
        let _ = self.event_tx.send(update);
    }

    fn handle_rest_result(&mut self, result: RestFetchResult) {
        let key = result.key;
        self.fetching.remove(&key);

        match result.result {
            Ok(snapshot) => {
                info!(
                    key = ?key,
                    best_bid = ?snapshot.best_bid,
                    best_ask = ?snapshot.best_ask,
                    "REST order-book snapshot fetched successfully"
                );
                self.cache.insert(key, snapshot.clone());
                // Deliver to pending callers.
                if let Some(waiters) = self.pending.remove(&key) {
                    debug!(key = ?key, waiters = waiters.len(), "Unblocking pending callers from REST result");
                    for tx in waiters {
                        let _ = tx.send(Ok(snapshot.clone()));
                    }
                }
                // Publish as an update.
                let _ = self.event_tx.send(OrderBookUpdate { snapshot });
            }
            Err(e) => {
                error!(key = ?key, error = %e, "REST order-book fetch failed");
                let reason = e.to_string();
                if let Some(waiters) = self.pending.remove(&key) {
                    for tx in waiters {
                        let _ = tx.send(Err(PriceManagerError::FeederError(PriceError::Network(
                            reason.clone(),
                        ))));
                    }
                }
            }
        }
    }

    fn maybe_start_rest_fetch(&mut self, key: InstrumentKey) {
        if !self.fetching.contains(&key) {
            self.start_rest_fetch(key);
        }
    }

    fn start_rest_fetch(&mut self, key: InstrumentKey) {
        if self.fetching.insert(key) {
            debug!(key = ?key, "Spawning background REST order-book fetch");
            let feeder = Arc::clone(&self.feeder);
            let tx = self.rest_result_tx.clone();
            tokio::spawn(async move {
                let result = feeder
                    .fetch_order_book(key)
                    .await
                    .map_err(PriceManagerError::FeederError);
                let _ = tx.send(RestFetchResult { key, result }).await;
            });
        }
    }
}

#[async_trait::async_trait]
impl PriceManager for PriceManagerHandle {
    async fn get_snapshot(
        &self,
        key: InstrumentKey,
    ) -> Result<OrderBookSnapshot, PriceManagerError> {
        let (reply_to, rx) = oneshot::channel();
        self.tx
            .send(PriceCommand::GetSnapshot { key, reply_to })
            .await
            .map_err(|_| PriceManagerError::ActorMailboxClosed)?;
        rx.await
            .map_err(|_| PriceManagerError::ActorResponseDropped)?
    }

    async fn refresh_snapshot(
        &self,
        key: InstrumentKey,
    ) -> Result<OrderBookSnapshot, PriceManagerError> {
        let (reply_to, rx) = oneshot::channel();
        self.tx
            .send(PriceCommand::RefreshSnapshot { key, reply_to })
            .await
            .map_err(|_| PriceManagerError::ActorMailboxClosed)?;
        rx.await
            .map_err(|_| PriceManagerError::ActorResponseDropped)?
    }

    fn subscribe_updates(&self) -> broadcast::Receiver<OrderBookUpdate> {
        self.event_tx.subscribe()
    }
}
