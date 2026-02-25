//! Bybit V5 exchange client.
//!
//! Implements `MarketDataProvider`, `FundingRateMarketData`, `OrderBookFeeder`,
//! `Account`, `AccountSnapshotSource`, `PositionProvider`, `LeverageProvider`,
//! and `OrderExecutionProvider`.

use crate::bybit::auth::BybitHmacAuth;
use crate::bybit::codec::BybitClientOrderIdCodec;
use crate::bybit::mappers::{
    bybit_order_to_order, bybit_position_to_position, funding_history_to_series,
    order_book_to_snapshot, ticker_to_funding_snapshot,
};
use crate::bybit::types::{
    BybitApiResponse, BybitFundingHistoryResult, BybitInstrumentsResult, BybitOrderBookResult,
    BybitOrderListResult, BybitPositionResult, BybitServerTime, BybitSwapMeta, BybitTickerResult,
    BybitWalletBalanceResult,
};
use crate::bybit::ws::BybitDepthPolicy;
use crate::bybit::ws::BybitUserDataPolicy;
use async_trait::async_trait;
use boldtrax_core::config::types::ExchangeConfig;
use boldtrax_core::http::{HttpClientBuilder, ResponseExt, TracedHttpClient};
use boldtrax_core::manager::account::{AccountManagerError, AccountSnapshotSource};
use boldtrax_core::manager::types::{
    AccountModel, AccountPartitionRef, AccountSnapshot, BalanceView, CollateralScope,
    PartitionKind, WsPositionPatch,
};
use boldtrax_core::registry::InstrumentRegistry;
use boldtrax_core::traits::{
    Account, AccountError, FundingRateMarketData, LeverageProvider, MarketDataError,
    MarketDataProvider, OrderBookFeeder, OrderExecutionProvider, PositionProvider, PriceError,
    TradingError,
};
use boldtrax_core::types::{
    Currency, Exchange, ExecutionMode, FundingInterval, FundingRateSeries, FundingRateSnapshot,
    Instrument, InstrumentKey, InstrumentType, Order, OrderBookSnapshot, OrderBookUpdate,
    OrderEvent, OrderRequest, OrderSide, OrderType, Position,
};
use boldtrax_core::ws::{CancellationToken, WsSupervisorHandle, ws_supervisor_spawn};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::RwLock;
use tokio::sync::{Mutex, OnceCell, mpsc};
use tracing::{debug, info, warn};

// ──────────────────────────────────────────────────────────────────
// Constants
// ──────────────────────────────────────────────────────────────────

const BYBIT_BASE_URL: &str = "https://api.bybit.com";
const BYBIT_TESTNET_URL: &str = "https://api-testnet.bybit.com";
const BYBIT_WS_PUBLIC_LINEAR: &str = "wss://stream.bybit.com/v5/public/linear";
const BYBIT_WS_PUBLIC_LINEAR_TESTNET: &str = "wss://stream-testnet.bybit.com/v5/public/linear";
const BYBIT_WS_PRIVATE: &str = "wss://stream.bybit.com/v5/private";
const BYBIT_WS_PRIVATE_TESTNET: &str = "wss://stream-testnet.bybit.com/v5/private";

// ──────────────────────────────────────────────────────────────────
// Config
// ──────────────────────────────────────────────────────────────────

fn default_timeout() -> u64 {
    30
}
fn default_retries() -> u32 {
    3
}
fn default_recv_window() -> u64 {
    5000
}
fn default_account_type() -> String {
    "UNIFIED".to_string()
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct BybitConfig {
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_retries")]
    pub max_retries: u32,
    pub api_key: Option<String>,
    pub api_secret: Option<String>,
    #[serde(default = "default_recv_window")]
    pub recv_window: u64,
    #[serde(default = "default_account_type")]
    pub account_type: String,
    #[serde(default)]
    pub testnet: bool,
    /// Instruments to track, e.g. `["BTCUSDT-BY-SWAP"]`.
    #[serde(default)]
    pub instruments: Vec<String>,
}

impl ExchangeConfig for BybitConfig {
    const EXCHANGE_NAME: &'static str = "bybit";

    fn validate(&self, execution_mode: ExecutionMode) {
        if execution_mode == ExecutionMode::Live {
            assert!(
                self.api_key.is_some(),
                "Bybit api_key is required for live trading"
            );
            assert!(
                self.api_secret.is_some(),
                "Bybit api_secret is required for live trading"
            );
        }
    }
}

impl BybitConfig {
    /// Resolve configured instrument strings into typed `InstrumentKey`s.
    pub fn tracked_keys(&self) -> Vec<InstrumentKey> {
        self.instruments
            .iter()
            .filter_map(|s| s.parse::<InstrumentKey>().ok())
            .collect()
    }
}

// ──────────────────────────────────────────────────────────────────
// Client
// ──────────────────────────────────────────────────────────────────

/// Shared handle for the private user-data WS supervisor.
///
/// Lazily spawned by `ensure_user_data()`.  Both `stream_executions` and
/// `stream_position_updates` take their respective rx halves from this handle.
struct BybitUserDataHandle {
    order_rx: Mutex<Option<mpsc::Receiver<OrderEvent>>>,
    pos_rx: Mutex<Option<mpsc::Receiver<WsPositionPatch>>>,
    _supervisor: WsSupervisorHandle,
}

pub struct BybitClient {
    config: BybitConfig,
    public_client: TracedHttpClient,
    private_client: Option<TracedHttpClient>,
    registry: InstrumentRegistry,
    instruments_loaded: OnceCell<()>,
    leverage_map: RwLock<HashMap<InstrumentKey, Decimal>>,
    user_data: OnceCell<BybitUserDataHandle>,
}

impl BybitClient {
    pub fn new(config: BybitConfig, registry: InstrumentRegistry) -> anyhow::Result<Self> {
        let base_url = if config.testnet {
            BYBIT_TESTNET_URL.to_string()
        } else {
            BYBIT_BASE_URL.to_string()
        };

        let public_client = HttpClientBuilder::new(base_url.clone())
            .timeout(config.timeout_secs)
            .max_retries(config.max_retries)
            .build()?;

        let has_credentials = config.api_key.is_some() && config.api_secret.is_some();
        let private_client = if has_credentials {
            let auth = BybitHmacAuth::new(
                config.api_key.clone().expect("API key is required"),
                config.api_secret.clone().expect("API secret is required"),
            )
            .with_recv_window_ms(config.recv_window);

            Some(
                HttpClientBuilder::new(base_url)
                    .timeout(config.timeout_secs)
                    .max_retries(config.max_retries)
                    .auth_provider(auth)
                    .build()?,
            )
        } else {
            None
        };

        Ok(Self {
            config,
            public_client,
            private_client,
            registry,
            instruments_loaded: OnceCell::new(),
            leverage_map: RwLock::new(HashMap::new()),
            user_data: OnceCell::new(),
        })
    }

    fn private_client(&self) -> Result<&TracedHttpClient, AccountError> {
        self.private_client.as_ref().ok_or_else(|| {
            AccountError::Other(
                "Bybit private client not configured (missing API keys)".to_string(),
            )
        })
    }

    /// Lazily spawn the shared private WS supervisor for order + position events.
    async fn ensure_user_data(&self) -> Result<&BybitUserDataHandle, AccountError> {
        self.user_data
            .get_or_try_init(|| async {
                let api_key = self.config.api_key.clone().ok_or_else(|| {
                    AccountError::Other("missing api_key for Bybit private WS".to_string())
                })?;
                let api_secret = self.config.api_secret.clone().ok_or_else(|| {
                    AccountError::Other("missing api_secret for Bybit private WS".to_string())
                })?;

                let ws_url = if self.config.testnet {
                    BYBIT_WS_PRIVATE_TESTNET
                } else {
                    BYBIT_WS_PRIVATE
                }
                .to_string();

                let (policy, channels) =
                    BybitUserDataPolicy::new(ws_url, api_key, api_secret, self.registry.clone());

                let cancel = CancellationToken::new();
                let supervisor = ws_supervisor_spawn(policy, cancel);
                info!("Bybit private user-data WS supervisor spawned (shared)");

                Ok(BybitUserDataHandle {
                    order_rx: Mutex::new(Some(channels.order_rx)),
                    pos_rx: Mutex::new(Some(channels.pos_rx)),
                    _supervisor: supervisor,
                })
            })
            .await
    }

    /// Fetch instruments with pagination (Bybit returns max 500 per page).
    async fn fetch_instruments(&self) -> Result<Vec<Instrument>, MarketDataError> {
        debug!("Fetching Bybit instruments info (linear)");
        let mut all_instruments = Vec::new();
        let mut cursor = String::new();

        loop {
            let path = if cursor.is_empty() {
                "/v5/market/instruments-info?category=linear&limit=1000".to_string()
            } else {
                format!(
                    "/v5/market/instruments-info?category=linear&limit=1000&cursor={}",
                    cursor
                )
            };

            let response = self.public_client.get(&path).await?;
            let api_resp: BybitApiResponse<BybitInstrumentsResult> = response
                .json_logged("Bybit instruments-info")
                .await
                .map_err(|e| MarketDataError::Parse(e.to_string()))?;

            if api_resp.ret_code != 0 {
                return Err(MarketDataError::Parse(format!(
                    "Bybit API error: {} (code {})",
                    api_resp.ret_msg, api_resp.ret_code
                )));
            }

            let page = api_resp.result;
            let page_count = page.list.len();

            let instruments: Vec<Instrument> = page
                .list
                .iter()
                .filter_map(|info| {
                    BybitSwapMeta::try_new(info)
                        .filter(|meta| meta.is_trading_allowed)
                        .map(Into::into)
                })
                .collect();

            all_instruments.extend(instruments);

            // Break if no more pages
            if page.next_page_cursor.is_empty() || page_count == 0 {
                break;
            }
            cursor = page.next_page_cursor;
        }

        debug!("Parsed {} Bybit instruments", all_instruments.len());
        Ok(all_instruments)
    }
}

// ──────────────────────────────────────────────────────────────────
// MarketDataProvider
// ──────────────────────────────────────────────────────────────────

#[async_trait]
impl MarketDataProvider for BybitClient {
    async fn load_instruments(&self) -> Result<(), MarketDataError> {
        self.instruments_loaded
            .get_or_try_init(|| async {
                info!("Loading Bybit instruments...");
                let instruments = self.fetch_instruments().await?;
                self.registry.insert_batch(instruments);
                info!("Bybit instruments loaded: {} total", self.registry.len());
                Ok::<(), MarketDataError>(())
            })
            .await
            .map(|_| ())
    }

    async fn health_check(&self) -> Result<(), MarketDataError> {
        let response = self.public_client.get("/v5/market/time").await?;
        let _: BybitApiResponse<BybitServerTime> = response
            .json_logged("Bybit health_check")
            .await
            .map_err(|e| MarketDataError::Parse(e.to_string()))?;
        debug!("Bybit health check OK");
        Ok(())
    }

    async fn server_time(&self) -> Result<DateTime<Utc>, MarketDataError> {
        let response = self.public_client.get("/v5/market/time").await?;
        let api_resp: BybitApiResponse<BybitServerTime> = response
            .json_logged("Bybit server time")
            .await
            .map_err(|e| MarketDataError::Parse(e.to_string()))?;

        if api_resp.ret_code != 0 {
            return Err(MarketDataError::Parse(format!(
                "Bybit server time error: {}",
                api_resp.ret_msg
            )));
        }

        // Convert timeSecond string to DateTime<Utc>
        let secs: i64 = api_resp
            .result
            .time_second
            .parse()
            .map_err(|_| MarketDataError::Parse("invalid server time seconds".to_string()))?;

        chrono::TimeZone::timestamp_opt(&Utc, secs, 0)
            .single()
            .ok_or_else(|| {
                MarketDataError::Parse("invalid server timestamp from Bybit".to_string())
            })
    }
}

// ──────────────────────────────────────────────────────────────────
// FundingRateMarketData
// ──────────────────────────────────────────────────────────────────

#[async_trait]
impl FundingRateMarketData for BybitClient {
    async fn funding_rate_snapshot(
        &self,
        key: InstrumentKey,
    ) -> Result<FundingRateSnapshot, MarketDataError> {
        if key.instrument_type != InstrumentType::Swap {
            return Err(MarketDataError::UnsupportedInstrument);
        }

        let instrument = self
            .registry
            .get(&key)
            .ok_or(MarketDataError::UnsupportedInstrument)?;
        let interval = instrument
            .funding_interval
            .unwrap_or(FundingInterval::Every8Hours);
        let symbol = instrument.exchange_symbol;

        let path = format!("/v5/market/tickers?category=linear&symbol={symbol}");
        debug!("Fetching Bybit ticker for {}", symbol);
        let response = self.public_client.get(&path).await?;
        let api_resp: BybitApiResponse<BybitTickerResult> = response
            .json_logged("Bybit tickers")
            .await
            .map_err(|e| MarketDataError::Parse(e.to_string()))?;

        if api_resp.ret_code != 0 {
            return Err(MarketDataError::Parse(format!(
                "Bybit ticker error: {}",
                api_resp.ret_msg
            )));
        }

        let ticker =
            api_resp.result.list.first().ok_or_else(|| {
                MarketDataError::Parse("empty ticker list from Bybit".to_string())
            })?;

        ticker_to_funding_snapshot(ticker, key, interval)
            .ok_or_else(|| MarketDataError::Parse("Failed to parse Bybit ticker".to_string()))
    }

    async fn funding_rate_history(
        &self,
        key: InstrumentKey,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        limit: usize,
    ) -> Result<FundingRateSeries, MarketDataError> {
        if key.instrument_type != InstrumentType::Swap {
            return Err(MarketDataError::UnsupportedInstrument);
        }

        let instrument = self
            .registry
            .get(&key)
            .ok_or(MarketDataError::UnsupportedInstrument)?;
        let interval = instrument
            .funding_interval
            .unwrap_or(FundingInterval::Every8Hours);
        let symbol = instrument.exchange_symbol;

        let start_ms = start.timestamp_millis();
        let end_ms = end.timestamp_millis();
        let limit = limit.min(200); // Bybit max is 200

        let path = format!(
            "/v5/market/funding/history?category=linear&symbol={symbol}&startTime={start_ms}&endTime={end_ms}&limit={limit}",
        );

        debug!(
            "Fetching Bybit funding history for {} from {} to {}",
            symbol, start, end
        );
        let response = self.public_client.get(&path).await?;
        let api_resp: BybitApiResponse<BybitFundingHistoryResult> = response
            .json_logged("Bybit funding history")
            .await
            .map_err(|e| MarketDataError::Parse(e.to_string()))?;

        if api_resp.ret_code != 0 {
            return Err(MarketDataError::Parse(format!(
                "Bybit funding history error: {}",
                api_resp.ret_msg
            )));
        }

        Ok(funding_history_to_series(
            &api_resp.result.list,
            key,
            interval,
        ))
    }
}

// ──────────────────────────────────────────────────────────────────
// OrderBookFeeder
// ──────────────────────────────────────────────────────────────────

#[async_trait]
impl OrderBookFeeder for BybitClient {
    async fn fetch_order_book(&self, key: InstrumentKey) -> Result<OrderBookSnapshot, PriceError> {
        let instrument = self
            .registry
            .get(&key)
            .ok_or(PriceError::UnsupportedInstrument)?;
        let symbol = instrument.exchange_symbol;

        debug!(key = ?key, symbol = %symbol, "Fetching Bybit REST order-book");

        if key.instrument_type != InstrumentType::Swap {
            return Err(PriceError::UnsupportedInstrument);
        }

        let path = format!("/v5/market/orderbook?category=linear&symbol={symbol}&limit=5");
        let resp = self
            .public_client
            .get(&path)
            .await
            .map_err(PriceError::from)?;
        let api_resp: BybitApiResponse<BybitOrderBookResult> = resp
            .json_logged("Bybit orderbook")
            .await
            .map_err(|e| PriceError::Parse(e.to_string()))?;

        if api_resp.ret_code != 0 {
            return Err(PriceError::Parse(format!(
                "Bybit orderbook error: {}",
                api_resp.ret_msg
            )));
        }

        let snapshot = order_book_to_snapshot(&api_resp.result, key)
            .ok_or_else(|| PriceError::Parse("failed to parse Bybit depth levels".to_string()))?;

        debug!(
            key = ?key,
            best_bid = ?snapshot.best_bid,
            best_ask = ?snapshot.best_ask,
            spread = ?snapshot.spread,
            "Bybit REST order-book snapshot parsed"
        );
        Ok(snapshot)
    }

    async fn stream_order_books(
        &self,
        keys: Vec<InstrumentKey>,
        tx: mpsc::Sender<OrderBookUpdate>,
    ) -> anyhow::Result<()> {
        let futures_keys: Vec<&InstrumentKey> = keys
            .iter()
            .filter(|k| k.instrument_type == InstrumentType::Swap)
            .collect();

        if futures_keys.is_empty() {
            return Ok(());
        }

        let cancel = CancellationToken::new();

        // Cancel when depth channel receiver drops.
        let cancel_on_close = cancel.clone();
        let tx_watcher = tx.clone();
        tokio::spawn(async move {
            tx_watcher.closed().await;
            cancel_on_close.cancel();
        });

        // Build symbol -> key map (uppercase symbols)
        let mut map = HashMap::new();
        for k in &futures_keys {
            if let Some(instrument) = self.registry.get(k) {
                // Bybit WS topic uses uppercase symbol: orderbook.1.BTCUSDT
                map.insert(instrument.exchange_symbol.clone(), **k);
            }
        }

        let ws_url = if self.config.testnet {
            BYBIT_WS_PUBLIC_LINEAR_TESTNET
        } else {
            BYBIT_WS_PUBLIC_LINEAR
        };

        let policy = BybitDepthPolicy {
            url: ws_url.to_string(),
            symbol_map: map,
            tx,
            depth: 1,
        };

        let handle = ws_supervisor_spawn(policy, cancel);
        handle.join().await;

        Ok(())
    }
}

// ──────────────────────────────────────────────────────────────────
// Account
// ──────────────────────────────────────────────────────────────────

#[async_trait]
impl Account for BybitClient {
    async fn account_snapshot(&self) -> Result<AccountSnapshot, AccountError> {
        let client = self.private_client()?;
        let as_of_utc = Utc::now();
        let account_type = &self.config.account_type;

        let path = format!("/v5/account/wallet-balance?accountType={account_type}");
        let response = client
            .get(&path)
            .await
            .map_err(|e| AccountError::Other(e.to_string()))?;
        let api_resp: BybitApiResponse<BybitWalletBalanceResult> = response
            .json_logged("Bybit wallet balance")
            .await
            .map_err(|e| AccountError::Parse(e.to_string()))?;

        if api_resp.ret_code != 0 {
            return Err(AccountError::Other(format!(
                "Bybit wallet error: {}",
                api_resp.ret_msg
            )));
        }

        let mut balances = Vec::new();
        let partition = AccountPartitionRef {
            exchange: Exchange::Bybit,
            kind: PartitionKind::Unified,
            account_id: None,
        };

        for account in &api_resp.result.list {
            for coin in &account.coin {
                let total = coin.wallet_balance.parse::<Decimal>().unwrap_or_default();
                if total.is_zero() {
                    continue;
                }
                let locked = coin.locked.parse::<Decimal>().unwrap_or_default();
                let free = total - locked;
                let unrealized_pnl = coin.unrealised_pnl.parse::<Decimal>().unwrap_or_default();

                if let Ok(asset) = coin.coin.parse::<Currency>() {
                    balances.push(BalanceView {
                        exchange: Exchange::Bybit,
                        partition: Some(partition.clone()),
                        asset,
                        total,
                        free,
                        locked,
                        borrowed: Decimal::ZERO,
                        unrealized_pnl,
                        collateral_scope: CollateralScope::SharedAcrossPartitions,
                        as_of_utc,
                    });
                }
            }
        }

        Ok(AccountSnapshot {
            exchange: Exchange::Bybit,
            model: AccountModel::Unified,
            partitions: vec![partition],
            balances,
            as_of_utc,
        })
    }
}

#[async_trait]
impl AccountSnapshotSource for BybitClient {
    async fn fetch_account_snapshot(
        &self,
        exchange: Exchange,
    ) -> Result<AccountSnapshot, AccountManagerError> {
        if exchange != Exchange::Bybit {
            return Err(AccountManagerError::Other {
                reason: format!("unsupported exchange for BybitClient: {exchange:?}"),
            });
        }

        self.account_snapshot()
            .await
            .map_err(AccountManagerError::SourceError)
    }
}

// ──────────────────────────────────────────────────────────────────
// PositionProvider
// ──────────────────────────────────────────────────────────────────

#[async_trait]
impl PositionProvider for BybitClient {
    async fn fetch_positions(&self) -> Result<Vec<Position>, TradingError> {
        let client = self.private_client()?;
        debug!("Fetching Bybit positions");

        let path = "/v5/position/list?category=linear&settleCoin=USDT&limit=200";
        let resp = client.get(path).await.map_err(TradingError::from)?;
        let api_resp: BybitApiResponse<BybitPositionResult> = resp
            .json_logged("Bybit positions")
            .await
            .map_err(|e| TradingError::Parse(e.to_string()))?;

        if api_resp.ret_code != 0 {
            return Err(TradingError::Parse(format!(
                "Bybit position error: {}",
                api_resp.ret_msg
            )));
        }

        let positions: Vec<Position> = api_resp
            .result
            .list
            .iter()
            .filter_map(|entry| {
                let inst = self.registry.get_by_exchange_symbol(
                    Exchange::Bybit,
                    &entry.symbol,
                    InstrumentType::Swap,
                )?;
                bybit_position_to_position(entry, inst.key)
            })
            .collect();

        // Apply cached leverage values
        let leverage_map = self
            .leverage_map
            .read()
            .expect("leverage_map lock poisoned");

        let positions_with_leverage = positions
            .into_iter()
            .map(|mut pos| {
                if let Some(&lev) = leverage_map.get(&pos.key) {
                    pos.leverage = lev;
                }
                pos
            })
            .collect();

        Ok(positions_with_leverage)
    }

    async fn stream_position_updates(
        &self,
        tx: mpsc::Sender<WsPositionPatch>,
    ) -> Result<(), AccountError> {
        let handle = self.ensure_user_data().await?;
        let mut pos_rx = match handle.pos_rx.lock().await.take() {
            Some(rx) => rx,
            None => {
                warn!("Bybit pos_rx already taken by a prior stream_position_updates call");
                std::future::pending::<()>().await;
                return Ok(());
            }
        };

        let cancel = CancellationToken::new();
        let cancel_on_close = cancel.clone();
        let tx_watcher = tx.clone();
        tokio::spawn(async move {
            tx_watcher.closed().await;
            cancel_on_close.cancel();
        });

        info!("Bybit position update relay started");
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                msg = pos_rx.recv() => {
                    match msg {
                        Some(patch) => {
                            if tx.send(patch).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
            }
        }

        Ok(())
    }
}

// ──────────────────────────────────────────────────────────────────
// LeverageProvider
// ──────────────────────────────────────────────────────────────────

#[async_trait]
impl LeverageProvider for BybitClient {
    async fn set_leverage(
        &self,
        key: InstrumentKey,
        leverage: Decimal,
    ) -> Result<Decimal, TradingError> {
        let instrument = self
            .registry
            .get(&key)
            .ok_or_else(|| TradingError::Other(format!("instrument not found: {key}")))?;
        let symbol = instrument.exchange_symbol;

        let client = self.private_client()?;

        // Bybit V5 set leverage is a POST with JSON body
        let body = serde_json::json!({
            "category": "linear",
            "symbol": symbol,
            "buyLeverage": leverage.to_string(),
            "sellLeverage": leverage.to_string(),
        });

        debug!(symbol = %symbol, leverage = %leverage, "Setting Bybit leverage");
        let resp = client
            .post("/v5/position/set-leverage", &body)
            .await
            .map_err(TradingError::from)?;

        // Bybit returns retCode=0 with empty result on success,
        // or retCode=110043 "Set leverage not modified" if already set.
        let api_resp: BybitApiResponse<serde_json::Value> = resp
            .json_logged("Bybit set_leverage")
            .await
            .map_err(|e| TradingError::Parse(e.to_string()))?;

        if api_resp.ret_code != 0 && api_resp.ret_code != 110043 {
            return Err(TradingError::Parse(format!(
                "Bybit set leverage error: {} (code {})",
                api_resp.ret_msg, api_resp.ret_code
            )));
        }

        info!(
            symbol = %symbol,
            requested = %leverage,
            "Leverage set on Bybit"
        );

        self.leverage_map
            .write()
            .expect("leverage_map lock poisoned")
            .insert(key, leverage);

        Ok(leverage)
    }

    async fn get_leverage(&self, key: InstrumentKey) -> Result<Option<Decimal>, TradingError> {
        let stored = self
            .leverage_map
            .read()
            .expect("leverage_map lock poisoned")
            .get(&key)
            .copied();

        Ok(stored)
    }
}

#[async_trait]
impl OrderExecutionProvider for BybitClient {
    fn encode_client_order_id(&self, internal_id: &str, strategy_id: &str) -> String {
        BybitClientOrderIdCodec.encode(internal_id, strategy_id)
    }

    fn decode_strategy_id(&self, client_order_id: &str) -> Option<String> {
        BybitClientOrderIdCodec.decode_strategy_id(client_order_id)
    }

    async fn place_order(
        &self,
        request: OrderRequest,
        client_order_id: String,
    ) -> Result<Order, TradingError> {
        let instrument = self
            .registry
            .get(&request.key)
            .ok_or(TradingError::UnsupportedInstrument)?;

        let symbol = instrument.exchange_symbol.clone();
        let side = match request.side {
            OrderSide::Buy => "Buy",
            OrderSide::Sell => "Sell",
        };
        let order_type = match request.order_type {
            OrderType::Market => "Market",
            OrderType::Limit => "Limit",
        };

        let size = instrument.normalize_quantity(request.size);

        let mut body = serde_json::json!({
            "category": "linear",
            "symbol": symbol,
            "side": side,
            "orderType": order_type,
            "qty": size.to_string(),
            "orderLinkId": client_order_id,
        });

        if let Some(price) = request.price {
            let price = instrument.normalize_price(price);
            body["price"] = serde_json::Value::String(price.to_string());
        }

        if request.post_only {
            body["timeInForce"] = serde_json::Value::String("PostOnly".to_string());
        } else if request.order_type == OrderType::Limit {
            body["timeInForce"] = serde_json::Value::String("GTC".to_string());
        }

        if request.reduce_only {
            body["reduceOnly"] = serde_json::Value::Bool(true);
        }

        let _key = request.key;
        let strategy_id = request.strategy_id.clone();
        let client = self.private_client()?;

        debug!(symbol = %symbol, side = side, order_type = order_type, "Placing Bybit order");
        let resp = client
            .post("/v5/order/create", &body)
            .await
            .map_err(TradingError::from)?;
        let api_resp: BybitApiResponse<serde_json::Value> = resp
            .json_logged("Bybit place_order")
            .await
            .map_err(|e| TradingError::Parse(e.to_string()))?;

        if api_resp.ret_code != 0 {
            return Err(TradingError::Parse(format!(
                "Bybit place order error: {} (code {})",
                api_resp.ret_msg, api_resp.ret_code
            )));
        }

        // Bybit place order only returns orderId + orderLinkId.
        // Build a minimal Order from what we know.
        let exchange_order_id = api_resp.result["orderId"].as_str().unwrap().to_string();

        let now = Utc::now();
        Ok(Order {
            internal_id: client_order_id.clone(),
            strategy_id,
            client_order_id,
            exchange_order_id: Some(exchange_order_id),
            request,
            status: boldtrax_core::types::OrderStatus::New,
            filled_size: Decimal::ZERO,
            avg_fill_price: None,
            created_at: now,
            updated_at: now,
        })
    }

    async fn cancel_order(
        &self,
        key: InstrumentKey,
        order_id: &str,
    ) -> Result<Order, TradingError> {
        let instrument = self
            .registry
            .get(&key)
            .ok_or(TradingError::UnsupportedInstrument)?;
        let symbol = instrument.exchange_symbol;

        let client = self.private_client()?;
        let body = serde_json::json!({
            "category": "linear",
            "symbol": symbol,
            "orderLinkId": order_id,
        });

        debug!(symbol = %symbol, order_id = order_id, "Canceling Bybit order");
        let resp = client
            .post("/v5/order/cancel", &body)
            .await
            .map_err(TradingError::from)?;
        let api_resp: BybitApiResponse<serde_json::Value> = resp
            .json_logged("Bybit cancel_order")
            .await
            .map_err(|e| TradingError::Parse(e.to_string()))?;

        if api_resp.ret_code != 0 {
            return Err(TradingError::Parse(format!(
                "Bybit cancel order error: {} (code {})",
                api_resp.ret_msg, api_resp.ret_code
            )));
        }

        // Return minimal canceled order
        let exchange_order_id = api_resp.result["orderId"].as_str().unwrap().to_string();

        let strategy_id = BybitClientOrderIdCodec
            .decode_strategy_id(order_id)
            .unwrap();
        let now = Utc::now();

        Ok(Order {
            internal_id: order_id.to_string(),
            strategy_id: strategy_id.clone(),
            client_order_id: order_id.to_string(),
            exchange_order_id: Some(exchange_order_id),
            request: OrderRequest {
                key,
                side: OrderSide::Buy,
                order_type: OrderType::Limit,
                price: None,
                size: Decimal::ZERO,
                post_only: false,
                reduce_only: false,
                strategy_id,
            },
            status: boldtrax_core::types::OrderStatus::Canceled,
            filled_size: Decimal::ZERO,
            avg_fill_price: None,
            created_at: now,
            updated_at: now,
        })
    }

    async fn get_open_orders(&self, key: InstrumentKey) -> Result<Vec<Order>, TradingError> {
        let instrument = self
            .registry
            .get(&key)
            .ok_or(TradingError::UnsupportedInstrument)?;
        let symbol = instrument.exchange_symbol;

        let client = self.private_client()?;
        let path =
            format!("/v5/order/realtime?category=linear&symbol={symbol}&openOnly=0&limit=50");
        debug!(symbol = %symbol, "Fetching Bybit open orders");
        let resp = client.get(&path).await.map_err(TradingError::from)?;
        let api_resp: BybitApiResponse<BybitOrderListResult> = resp
            .json_logged("Bybit get_open_orders")
            .await
            .map_err(|e| TradingError::Parse(e.to_string()))?;

        if api_resp.ret_code != 0 {
            return Err(TradingError::Parse(format!(
                "Bybit open orders error: {} (code {})",
                api_resp.ret_msg, api_resp.ret_code
            )));
        }

        Ok(api_resp
            .result
            .list
            .iter()
            .filter_map(|entry| bybit_order_to_order(entry, key))
            .collect())
    }

    async fn get_order_status(
        &self,
        key: InstrumentKey,
        order_id: &str,
    ) -> Result<Order, TradingError> {
        let instrument = self
            .registry
            .get(&key)
            .ok_or(TradingError::UnsupportedInstrument)?;
        let symbol = instrument.exchange_symbol;

        let client = self.private_client()?;
        let path =
            format!("/v5/order/realtime?category=linear&symbol={symbol}&orderLinkId={order_id}");
        debug!(symbol = %symbol, order_id = order_id, "Querying Bybit order status");
        let resp = client.get(&path).await.map_err(TradingError::from)?;
        let api_resp: BybitApiResponse<BybitOrderListResult> = resp
            .json_logged("Bybit get_order_status")
            .await
            .map_err(|e| TradingError::Parse(e.to_string()))?;

        if api_resp.ret_code != 0 {
            return Err(TradingError::Parse(format!(
                "Bybit order status error: {} (code {})",
                api_resp.ret_msg, api_resp.ret_code
            )));
        }

        api_resp
            .result
            .list
            .first()
            .and_then(|entry| bybit_order_to_order(entry, key))
            .ok_or_else(|| TradingError::Parse("Bybit order not found".to_string()))
    }

    async fn stream_executions(&self, tx: mpsc::Sender<OrderEvent>) -> Result<(), AccountError> {
        let handle = self.ensure_user_data().await?;
        let mut order_rx = match handle.order_rx.lock().await.take() {
            Some(rx) => rx,
            None => {
                warn!("Bybit order_rx already taken by a prior stream_executions call");
                std::future::pending::<()>().await;
                return Ok(());
            }
        };

        let cancel = CancellationToken::new();
        let cancel_on_close = cancel.clone();
        let tx_watcher = tx.clone();
        tokio::spawn(async move {
            tx_watcher.closed().await;
            cancel_on_close.cancel();
        });

        info!("Bybit order execution relay started");
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                msg = order_rx.recv() => {
                    match msg {
                        Some(event) => {
                            if tx.send(event).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
            }
        }

        Ok(())
    }
}
