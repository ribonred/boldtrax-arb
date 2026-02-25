use crate::aster::auth::AsterHmacAuth;
use async_trait::async_trait;
use boldtrax_core::ExecutionMode;
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
    Currency, Exchange, FundingInterval, FundingRateSeries, FundingRateSnapshot, Instrument,
    InstrumentKey, InstrumentType, Order, OrderBookSnapshot, OrderBookUpdate, OrderEvent,
    OrderRequest, OrderSide, OrderType, Position,
};
use boldtrax_core::ws::{CancellationToken, WsSupervisorHandle, ws_supervisor_spawn};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::RwLock;
use tokio::sync::{Mutex, OnceCell, mpsc};
use tracing::{debug, info, warn};

use crate::aster::codec::AsterClientOrderIdCodec;
use crate::aster::mappers::{
    aster_order_response_to_order, aster_position_risk_to_position, funding_history_to_series,
    ms_to_datetime, partial_depth_to_order_book, premium_index_to_snapshot,
};
use crate::aster::types::{
    AsterExchangeInfo, AsterFundingInfo, AsterFundingRateHistory, AsterFuturesPositionRisk,
    AsterLeverageResponse, AsterOrderResponse, AsterPartialDepth, AsterPremiumIndex,
    AsterServerTime, AsterSwapMeta,
};
use crate::aster::ws::AsterDepthPolicy;
use crate::aster::ws::AsterUserDataPolicy;

struct UserDataHandle {
    order_rx: Mutex<Option<mpsc::Receiver<OrderEvent>>>,
    pos_rx: Mutex<Option<mpsc::Receiver<WsPositionPatch>>>,
    _supervisor: WsSupervisorHandle,
}

const ASTER_FUTURES_BASE_URL: &str = "https://fapi.asterdex.com";
const ASTER_FUTURES_TESTNET_URL: &str = "https://fapi.asterdex-testnet.com";
const ASTER_FUTURES_WS_BASE_URL: &str = "wss://fstream.asterdex.com";
const ASTER_FUTURES_WS_TESTNET_URL: &str = "wss://fstream.asterdex-testnet.com";

fn default_timeout() -> u64 {
    30
}
fn default_retries() -> u32 {
    3
}
fn default_recv_window() -> u64 {
    5000
}

/// Aster client configuration
#[derive(Debug, Clone, Deserialize, Default)]
pub struct AsterConfig {
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_retries")]
    pub max_retries: u32,
    pub api_key: Option<String>,
    pub api_secret: Option<String>,
    #[serde(default = "default_recv_window")]
    pub recv_window: u64,
    /// Custom base URL for futures API (for testing)
    pub futures_base_url: Option<String>,
    #[serde(default)]
    pub testnet: bool,
    /// Instruments to track for price feeds and funding-rate polling.
    /// Each entry is an `InstrumentKey` string, e.g. `"SOLUSDT-AS-SWAP"`.
    #[serde(default)]
    pub instruments: Vec<String>,
}

impl ExchangeConfig for AsterConfig {
    const EXCHANGE_NAME: &'static str = "aster";

    fn validate(&self, execution_mode: ExecutionMode) {
        if execution_mode == ExecutionMode::Live {
            assert!(
                self.api_key.is_some(),
                "Aster api_key is required for live trading"
            );
            assert!(
                self.api_secret.is_some(),
                "Aster api_secret is required for live trading"
            );
        }
    }
}

impl AsterConfig {
    /// Resolve configured instrument strings into typed `InstrumentKey`s.
    /// Invalid entries are silently skipped.
    pub fn tracked_keys(&self) -> Vec<InstrumentKey> {
        self.instruments
            .iter()
            .filter_map(|s| s.parse::<InstrumentKey>().ok())
            .collect()
    }
}

pub struct AsterClient {
    config: AsterConfig,
    public_client: TracedHttpClient,
    private_client: Option<TracedHttpClient>,
    registry: InstrumentRegistry,
    instruments_loaded: OnceCell<()>,
    leverage_map: RwLock<HashMap<InstrumentKey, rust_decimal::Decimal>>,
    user_data: OnceCell<UserDataHandle>,
}

impl AsterClient {
    pub fn new(config: AsterConfig, registry: InstrumentRegistry) -> anyhow::Result<Self> {
        let base_url = if let Some(url) = &config.futures_base_url {
            url.clone()
        } else if config.testnet {
            ASTER_FUTURES_TESTNET_URL.to_string()
        } else {
            ASTER_FUTURES_BASE_URL.to_string()
        };

        let public_client = HttpClientBuilder::new(base_url.clone())
            .timeout(config.timeout_secs)
            .max_retries(config.max_retries)
            .build()?;

        let has_private_credentials = config.api_key.is_some() && config.api_secret.is_some();
        let private_client = if has_private_credentials {
            let auth = AsterHmacAuth::new(
                config.api_key.clone().unwrap_or_default(),
                config.api_secret.clone().unwrap_or_default(),
            );

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

    async fn fetch_instruments(&self) -> Result<Vec<Instrument>, MarketDataError> {
        debug!("Fetching Aster exchange info");
        let response = self.public_client.get("/fapi/v3/exchangeInfo").await?;
        let info: AsterExchangeInfo = response
            .json_logged("Aster exchangeInfo")
            .await
            .map_err(|e| MarketDataError::Parse(e.to_string()))?;

        debug!("Fetching Aster funding info");
        let funding_response = self.public_client.get("/fapi/v3/fundingInfo").await?;
        let funding_info: Vec<AsterFundingInfo> = funding_response
            .json_logged("Aster fundingInfo")
            .await
            .map_err(|e| MarketDataError::Parse(e.to_string()))?;

        let funding_map: HashMap<&str, u8> = funding_info
            .iter()
            .map(|f| (f.symbol.as_str(), f.funding_interval_hours))
            .collect();

        let instruments: Vec<Instrument> = info
            .symbols
            .iter()
            .filter_map(|symbol| {
                let funding_hours = funding_map.get(symbol.symbol.as_str()).copied();
                AsterSwapMeta::try_new(symbol, funding_hours)
                    .filter(|meta| meta.is_trading_allowed)
                    .map(Into::into)
            })
            .collect();

        debug!("Parsed {} Aster instruments", instruments.len());

        Ok(instruments)
    }
}

#[async_trait]
impl MarketDataProvider for AsterClient {
    async fn load_instruments(&self) -> Result<(), MarketDataError> {
        self.instruments_loaded
            .get_or_try_init(|| async {
                info!("Loading Aster instruments...");
                let instruments = self.fetch_instruments().await?;
                self.registry.insert_batch(instruments);
                info!("Aster instruments loaded: {} total", self.registry.len());
                Ok::<(), MarketDataError>(())
            })
            .await
            .map(|_| ())
    }

    async fn health_check(&self) -> Result<(), MarketDataError> {
        self.public_client.get("/fapi/v3/ping").await?;
        debug!("Aster health check OK");
        Ok(())
    }

    async fn server_time(&self) -> Result<DateTime<Utc>, MarketDataError> {
        let response = self.public_client.get("/fapi/v3/time").await?;
        let time: AsterServerTime = response
            .json_logged("Aster server time")
            .await
            .map_err(|e| MarketDataError::Parse(e.to_string()))?;

        Ok(ms_to_datetime(time.server_time).ok_or_else(|| {
            MarketDataError::Parse("invalid server timestamp from Aster".to_string())
        })?)
    }
}

#[async_trait]
impl FundingRateMarketData for AsterClient {
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
        let path = format!("/fapi/v3/premiumIndex?symbol={symbol}");

        debug!("Fetching premium index for {}", symbol);
        let response = self.public_client.get(&path).await?;
        let index: AsterPremiumIndex = response
            .json_logged("Aster premiumIndex")
            .await
            .map_err(|e| MarketDataError::Parse(e.to_string()))?;

        premium_index_to_snapshot(&index, key.pair, interval)
            .ok_or_else(|| MarketDataError::Parse("Failed to parse premium index".to_string()))
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
        let limit = limit.min(1000);

        let path = format!(
            "/fapi/v3/fundingRate?symbol={symbol}&startTime={start_ms}&endTime={end_ms}&limit={limit}",
        );

        debug!(
            "Fetching funding rate history for {} from {} to {}",
            symbol, start, end
        );
        let response = self.public_client.get(&path).await?;
        let history: Vec<AsterFundingRateHistory> = response
            .json_logged("Aster fundingRate")
            .await
            .map_err(|e| MarketDataError::Parse(e.to_string()))?;

        Ok(funding_history_to_series(&history, key.pair, interval))
    }
}

#[async_trait]
impl OrderBookFeeder for AsterClient {
    async fn fetch_order_book(&self, key: InstrumentKey) -> Result<OrderBookSnapshot, PriceError> {
        let instrument = self
            .registry
            .get(&key)
            .ok_or(PriceError::UnsupportedInstrument)?;
        let symbol = instrument.exchange_symbol;
        let timestamp_ms = Utc::now().timestamp_millis();

        debug!(key = ?key, symbol = %symbol, "Fetching REST order-book snapshot");

        if key.instrument_type != InstrumentType::Swap {
            return Err(PriceError::UnsupportedInstrument);
        }

        let endpoint = format!("/fapi/v3/depth?symbol={symbol}&limit=5");
        let resp = self
            .public_client
            .get(&endpoint)
            .await
            .map_err(PriceError::from)?;
        let depth: AsterPartialDepth = resp
            .json_logged("Aster depth")
            .await
            .map_err(|e| PriceError::Parse(e.to_string()))?;
        let snapshot = partial_depth_to_order_book(&depth, key, timestamp_ms)
            .ok_or_else(|| PriceError::Parse("failed to parse depth levels".to_string()))?;
        debug!(
            key = ?key,
            best_bid = ?snapshot.best_bid,
            best_ask = ?snapshot.best_ask,
            spread = ?snapshot.spread,
            "Aster REST order-book snapshot parsed"
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

        // Cancel all depth supervisors when the depth channel receiver is dropped.
        let cancel_on_close = cancel.clone();
        let tx_watcher = tx.clone();
        tokio::spawn(async move {
            tx_watcher.closed().await;
            cancel_on_close.cancel();
        });

        let mut map = HashMap::new();
        for k in &futures_keys {
            if let Some(instrument) = self.registry.get(k) {
                let symbol_lower = instrument.exchange_symbol.to_lowercase();
                map.insert(symbol_lower, **k);
            }
        }

        let base_url = if self.config.testnet {
            ASTER_FUTURES_WS_TESTNET_URL
        } else {
            ASTER_FUTURES_WS_BASE_URL
        };

        let streams: Vec<String> = map.keys().map(|s| format!("{}@depth5@100ms", s)).collect();
        let url = format!("{}/stream?streams={}", base_url, streams.join("/"));

        let policy = AsterDepthPolicy {
            url,
            symbol_map: map,
            tx,
        };

        let handle = ws_supervisor_spawn(policy, cancel);
        handle.join().await;

        Ok(())
    }
}

impl AsterClient {
    fn private_client(&self) -> Result<&TracedHttpClient, AccountError> {
        self.private_client.as_ref().ok_or_else(|| {
            AccountError::Other(
                "Aster private client not configured (missing API keys)".to_string(),
            )
        })
    }

    async fn ensure_user_data(&self) -> Result<&UserDataHandle, AccountError> {
        self.user_data
            .get_or_try_init(|| async {
                let client = self.private_client()?.clone();
                let base_ws_url = if self.config.testnet {
                    ASTER_FUTURES_WS_TESTNET_URL
                } else {
                    ASTER_FUTURES_WS_BASE_URL
                }
                .to_string();

                let (policy, channels) =
                    AsterUserDataPolicy::new(client, base_ws_url, self.registry.clone());

                let cancel = CancellationToken::new();
                let supervisor = ws_supervisor_spawn(policy, cancel);
                info!("Aster user-data supervisor spawned (shared)");

                Ok(UserDataHandle {
                    order_rx: Mutex::new(Some(channels.order_rx)),
                    pos_rx: Mutex::new(Some(channels.pos_rx)),
                    _supervisor: supervisor,
                })
            })
            .await
    }

    async fn fetch_futures_balances(
        &self,
    ) -> Result<Vec<crate::aster::types::AsterFuturesBalance>, AccountError> {
        let client = self.private_client()?;
        let response = client
            .get("/fapi/v2/balance")
            .await
            .map_err(|e| AccountError::Other(e.to_string()))?;
        let balances: Vec<crate::aster::types::AsterFuturesBalance> = response
            .json_logged("Aster futures balance")
            .await
            .map_err(|e| AccountError::Parse(e.to_string()))?;
        Ok(balances)
    }
}

#[async_trait]
impl Account for AsterClient {
    async fn account_snapshot(&self) -> Result<AccountSnapshot, AccountError> {
        let as_of_utc = Utc::now();
        let futures_balances = self.fetch_futures_balances().await?;

        let mut balances = Vec::new();
        let partition = AccountPartitionRef {
            exchange: Exchange::Aster,
            kind: PartitionKind::Swap,
            account_id: None,
        };

        for b in futures_balances {
            let total = b
                .balance
                .parse::<rust_decimal::Decimal>()
                .unwrap_or_default();
            let free = b
                .available_balance
                .parse::<rust_decimal::Decimal>()
                .unwrap_or_default();
            let unrealized_pnl = b
                .cross_un_pnl
                .parse::<rust_decimal::Decimal>()
                .unwrap_or_default();
            let locked = total - free;

            if let Ok(asset) = b.asset.parse::<Currency>() {
                balances.push(BalanceView {
                    exchange: Exchange::Aster,
                    partition: Some(partition.clone()),
                    asset,
                    total,
                    free,
                    locked,
                    borrowed: rust_decimal::Decimal::ZERO,
                    unrealized_pnl,
                    collateral_scope: CollateralScope::PartitionOnly,
                    as_of_utc,
                });
            }
        }

        Ok(AccountSnapshot {
            exchange: Exchange::Aster,
            model: AccountModel::Segmented,
            partitions: vec![partition],
            balances,
            as_of_utc,
        })
    }
}

#[async_trait]
impl AccountSnapshotSource for AsterClient {
    async fn fetch_account_snapshot(
        &self,
        exchange: Exchange,
    ) -> Result<AccountSnapshot, AccountManagerError> {
        if exchange != Exchange::Aster {
            return Err(AccountManagerError::Other {
                reason: format!("unsupported exchange for AsterClient: {exchange:?}"),
            });
        }

        self.account_snapshot()
            .await
            .map_err(AccountManagerError::SourceError)
    }
}

#[async_trait]
impl LeverageProvider for AsterClient {
    async fn set_leverage(
        &self,
        key: InstrumentKey,
        leverage: rust_decimal::Decimal,
    ) -> Result<rust_decimal::Decimal, TradingError> {
        let instrument = self
            .registry
            .get(&key)
            .ok_or_else(|| TradingError::Other(format!("instrument not found: {key}")))?;
        let symbol = instrument.exchange_symbol;

        let client = self.private_client()?;
        let leverage_int = leverage.to_string();
        let path = format!("/fapi/v1/leverage?symbol={symbol}&leverage={leverage_int}");

        debug!(symbol = %symbol, leverage = %leverage, "Setting Aster leverage");
        let resp = client.post_empty(&path).await.map_err(TradingError::from)?;
        let result: AsterLeverageResponse = resp
            .json_logged("Aster leverage")
            .await
            .map_err(|e| TradingError::Parse(e.to_string()))?;

        let actual = rust_decimal::Decimal::from(result.leverage);
        info!(
            symbol = %symbol,
            requested = %leverage,
            actual = %actual,
            max_notional = %result.max_notional_value,
            "Leverage set on Aster"
        );

        self.leverage_map
            .write()
            .expect("leverage_map lock poisoned")
            .insert(key, actual);

        Ok(actual)
    }

    async fn get_leverage(
        &self,
        key: InstrumentKey,
    ) -> Result<Option<rust_decimal::Decimal>, TradingError> {
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
impl OrderExecutionProvider for AsterClient {
    fn encode_client_order_id(&self, internal_id: &str, strategy_id: &str) -> String {
        AsterClientOrderIdCodec.encode(internal_id, strategy_id)
    }

    fn decode_strategy_id(&self, client_order_id: &str) -> Option<String> {
        AsterClientOrderIdCodec.decode_strategy_id(client_order_id)
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
            OrderSide::Buy => "BUY",
            OrderSide::Sell => "SELL",
        };
        let (order_type, time_in_force) = match request.order_type {
            OrderType::Market => ("MARKET", None),
            OrderType::Limit if request.post_only => ("POST_ONLY", None),
            OrderType::Limit => ("LIMIT", Some("GTC")),
        };

        let size = instrument.normalize_quantity(request.size);

        let mut params = format!(
            "symbol={symbol}&side={side}&type={order_type}&quantity={size}&newClientOrderId={client_order_id}",
        );

        if let Some(price) = request.price {
            let price = instrument.normalize_price(price);
            params.push_str(&format!("&price={price}"));
        }

        if let Some(tif) = time_in_force {
            params.push_str(&format!("&timeInForce={tif}"));
        }

        if request.reduce_only {
            params.push_str("&reduceOnly=true");
        }

        let key = request.key;
        let strategy_id = request.strategy_id.clone();

        let client = self.private_client()?;
        let path = format!("/fapi/v1/order?{params}");
        debug!(symbol = %symbol, side = side, order_type = order_type, "Placing Aster order");
        let resp = client.post_empty(&path).await.map_err(TradingError::from)?;
        let order_resp: AsterOrderResponse = resp.json_logged("Aster place_order").await?;
        aster_order_response_to_order(&order_resp, key, &strategy_id, &client_order_id).ok_or_else(
            || TradingError::Parse("failed to parse Aster place_order response".to_string()),
        )
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
        let path = format!("/fapi/v1/order?symbol={symbol}&origClientOrderId={order_id}");
        debug!(symbol = %symbol, order_id = order_id, "Canceling Aster order");
        let resp = client.delete(&path).await.map_err(TradingError::from)?;
        let order_resp: AsterOrderResponse = resp.json_logged("Aster cancel_order").await?;
        let strategy_id = AsterClientOrderIdCodec
            .decode_strategy_id(&order_resp.client_order_id)
            .unwrap_or_default();
        aster_order_response_to_order(&order_resp, key, &strategy_id, order_id).ok_or_else(|| {
            TradingError::Parse("failed to parse Aster cancel_order response".to_string())
        })
    }

    async fn get_open_orders(&self, key: InstrumentKey) -> Result<Vec<Order>, TradingError> {
        let instrument = self
            .registry
            .get(&key)
            .ok_or(TradingError::UnsupportedInstrument)?;
        let symbol = instrument.exchange_symbol;

        let client = self.private_client()?;
        let path = format!("/fapi/v1/openOrders?symbol={symbol}");
        debug!(symbol = %symbol, "Fetching Aster open orders");
        let resp = client.get(&path).await.map_err(TradingError::from)?;
        let orders: Vec<AsterOrderResponse> = resp.json_logged("Aster get_open_orders").await?;
        Ok(orders
            .iter()
            .filter_map(|o| {
                let strategy_id = AsterClientOrderIdCodec
                    .decode_strategy_id(&o.client_order_id)
                    .unwrap();
                aster_order_response_to_order(o, key, &strategy_id, &o.client_order_id)
            })
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
        let path = format!("/fapi/v1/order?symbol={symbol}&origClientOrderId={order_id}");
        debug!(symbol = %symbol, order_id = order_id, "Querying Aster order status");
        let resp = client.get(&path).await.map_err(TradingError::from)?;
        let order_resp: AsterOrderResponse = resp.json_logged("Aster get_order_status").await?;
        let strategy_id = AsterClientOrderIdCodec
            .decode_strategy_id(&order_resp.client_order_id)
            .unwrap_or_default();
        aster_order_response_to_order(&order_resp, key, &strategy_id, &order_resp.client_order_id)
            .ok_or_else(|| {
                TradingError::Parse("failed to parse Aster order status response".to_string())
            })
    }

    async fn stream_executions(&self, tx: mpsc::Sender<OrderEvent>) -> Result<(), AccountError> {
        let handle = self.ensure_user_data().await?;
        let mut order_rx = match handle.order_rx.lock().await.take() {
            Some(rx) => rx,
            None => {
                warn!("Aster order_rx already taken by a prior stream_executions call");
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

#[async_trait]
impl PositionProvider for AsterClient {
    async fn fetch_positions(&self) -> Result<Vec<Position>, TradingError> {
        let client = self.private_client()?;
        debug!("Fetching Aster Futures position risk");
        let resp = client
            .get("/fapi/v2/positionRisk")
            .await
            .map_err(TradingError::from)?;
        let risks: Vec<AsterFuturesPositionRisk> = resp
            .json_logged("Aster positionRisk")
            .await
            .map_err(|e| TradingError::Parse(e.to_string()))?;

        let positions: Vec<Position> = risks
            .iter()
            .filter_map(|risk| {
                let instrument = self.registry.get_by_exchange_symbol(
                    Exchange::Aster,
                    &risk.symbol,
                    InstrumentType::Swap,
                )?;
                aster_position_risk_to_position(risk, instrument.key)
            })
            .collect();

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
                warn!("Aster pos_rx already taken by a prior stream_position_updates call");
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
