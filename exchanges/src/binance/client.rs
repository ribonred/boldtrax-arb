//! Binance exchange client

use async_trait::async_trait;
use boldtrax_core::http::{BinanceHmacAuth, HttpClientBuilder, TracedHttpClient};
use boldtrax_core::manager::account::{AccountManagerError, AccountSnapshotSource};
use boldtrax_core::registry::InstrumentRegistry;
use boldtrax_core::traits::{
    Account, AccountError, BaseInstrumentMeta, FundingRateMarketData, MarketDataError,
    MarketDataProvider, OrderBookFeeder, OrderExecutionProvider, PositionProvider, PriceError,
    TradingError,
};
use boldtrax_core::types::{
    FundingInterval, FundingRateSeries, FundingRateSnapshot, Instrument, InstrumentKey,
    InstrumentType, Order, OrderBookSnapshot, OrderBookUpdate, OrderRequest,
};
use boldtrax_core::ws::{CancellationToken, ws_supervisor_spawn};
use boldtrax_core::{AccountSnapshot, Exchange, OrderEvent, Position};
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use tokio::sync::{OnceCell, mpsc};
use tracing::{debug, info};

use crate::binance::ws::{
    BinanceDepthPolicy, BinanceFuturesUserDataPolicy, BinanceSpotUserDataPolicy,
};

use crate::binance::mappers::{
    build_segmented_account_snapshot, funding_history_to_series, ms_to_datetime,
    partial_depth_to_order_book, premium_index_to_snapshot,
};
use crate::binance::types::{
    BinanceFundingInfo, BinanceFundingRateHistory, BinanceFuturesBalance,
    BinanceFuturesExchangeInfo, BinancePartialDepth, BinancePremiumIndex, BinanceServerTime,
    BinanceSpotAccount, BinanceSpotBalance, BinanceSpotExchangeInfo, BinanceSpotMeta,
    BinanceSwapMeta,
};

use boldtrax_core::config::types::ExchangeConfig;
use serde::Deserialize;

const BINANCE_FUTURES_BASE_URL: &str = "https://fapi.binance.com";
const BINANCE_SPOT_BASE_URL: &str = "https://api.binance.com";
const BINANCE_FUTURES_TESTNET_URL: &str = "https://testnet.binancefuture.com";
const BINANCE_SPOT_TESTNET_URL: &str = "https://testnet.binance.vision";
const BINANCE_FUTURES_WS_BASE_URL: &str = "wss://fstream.binance.com";
const BINANCE_SPOT_WS_BASE_URL: &str = "wss://stream.binance.com:9443";
const BINANCE_FUTURES_WS_TESTNET_URL: &str = "wss://stream.binancefuture.com";
const BINANCE_SPOT_WS_TESTNET_URL: &str = "wss://testnet.binance.vision:443";
/// Binance WebSocket API endpoint for authenticated Spot user-data streams.
const BINANCE_SPOT_WS_API_URL: &str = "wss://ws-api.binance.com:443/ws-api/v3";

/// Binance API mode configuration
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BinanceApiMode {
    Futures,
    Spot,
    #[default]
    Both,
}

fn default_timeout() -> u64 {
    30
}
fn default_retries() -> u32 {
    3
}

/// Binance client configuration
#[derive(Debug, Clone, Deserialize)]
pub struct BinanceConfig {
    #[serde(default)]
    pub mode: BinanceApiMode,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_retries")]
    pub max_retries: u32,
    pub api_key: Option<String>,
    pub api_secret: Option<String>,
    /// Custom base URL for futures API (for testing)
    pub futures_base_url: Option<String>,
    /// Custom base URL for spot API (for testing)
    pub spot_base_url: Option<String>,
    #[serde(default)]
    pub testnet: bool,
}

impl ExchangeConfig for BinanceConfig {
    const EXCHANGE_NAME: &'static str = "binance";
}

impl Default for BinanceConfig {
    fn default() -> Self {
        Self {
            mode: BinanceApiMode::Both,
            timeout_secs: default_timeout(),
            max_retries: default_retries(),
            api_key: None,
            api_secret: None,
            futures_base_url: None,
            spot_base_url: None,
            testnet: false,
        }
    }
}

/// Binance exchange client
pub struct BinanceClient {
    #[allow(dead_code)]
    config: BinanceConfig,
    registry: InstrumentRegistry,
    futures_public_client: Option<TracedHttpClient>,
    spot_public_client: Option<TracedHttpClient>,
    futures_private_client: Option<TracedHttpClient>,
    spot_private_client: Option<TracedHttpClient>,
    /// Ensures `load_instruments` only fetches once even under concurrent calls
    instruments_loaded: OnceCell<()>,
}

impl BinanceClient {
    pub fn new(
        config: BinanceConfig,
        registry: InstrumentRegistry,
    ) -> Result<Self, MarketDataError> {
        let futures_base_url = config.futures_base_url.clone().unwrap_or_else(|| {
            if config.testnet {
                BINANCE_FUTURES_TESTNET_URL.to_string()
            } else {
                BINANCE_FUTURES_BASE_URL.to_string()
            }
        });
        let spot_base_url = config.spot_base_url.clone().unwrap_or_else(|| {
            if config.testnet {
                BINANCE_SPOT_TESTNET_URL.to_string()
            } else {
                BINANCE_SPOT_BASE_URL.to_string()
            }
        });

        let futures_public_client =
            if matches!(config.mode, BinanceApiMode::Futures | BinanceApiMode::Both) {
                Some(
                    HttpClientBuilder::new(futures_base_url.clone())
                        .timeout(config.timeout_secs)
                        .max_retries(config.max_retries)
                        .build()
                        .map_err(|e| MarketDataError::Other(e.to_string()))?,
                )
            } else {
                None
            };

        let spot_public_client =
            if matches!(config.mode, BinanceApiMode::Spot | BinanceApiMode::Both) {
                Some(
                    HttpClientBuilder::new(spot_base_url.clone())
                        .timeout(config.timeout_secs)
                        .max_retries(config.max_retries)
                        .build()
                        .map_err(|e| MarketDataError::Other(e.to_string()))?,
                )
            } else {
                None
            };

        let has_private_credentials = config.api_key.is_some() && config.api_secret.is_some();

        let futures_private_client = if has_private_credentials
            && matches!(config.mode, BinanceApiMode::Futures | BinanceApiMode::Both)
        {
            let auth = BinanceHmacAuth::new(
                config.api_key.clone().unwrap_or_default(),
                config.api_secret.clone().unwrap_or_default(),
            );

            Some(
                HttpClientBuilder::new(futures_base_url)
                    .timeout(config.timeout_secs)
                    .max_retries(config.max_retries)
                    .auth_provider(auth)
                    .build()
                    .map_err(|e| MarketDataError::Other(e.to_string()))?,
            )
        } else {
            None
        };

        let spot_private_client = if has_private_credentials
            && matches!(config.mode, BinanceApiMode::Spot | BinanceApiMode::Both)
        {
            let auth = BinanceHmacAuth::new(
                config.api_key.clone().unwrap_or_default(),
                config.api_secret.clone().unwrap_or_default(),
            );

            Some(
                HttpClientBuilder::new(spot_base_url)
                    .timeout(config.timeout_secs)
                    .max_retries(config.max_retries)
                    .auth_provider(auth)
                    .build()
                    .map_err(|e| MarketDataError::Other(e.to_string()))?,
            )
        } else {
            None
        };

        Ok(Self {
            config,
            registry,
            futures_public_client,
            spot_public_client,
            futures_private_client,
            spot_private_client,
            instruments_loaded: OnceCell::new(),
        })
    }

    async fn fetch_spot_balances(&self) -> Result<Vec<BinanceSpotBalance>, AccountError> {
        let client = self.spot_private_client()?;
        let response = client.get("/api/v3/account?omitZeroBalances=true").await?;
        let account: BinanceSpotAccount = response
            .json()
            .await
            .map_err(|e| AccountError::Parse(e.to_string()))?;
        Ok(account.balances)
    }

    async fn fetch_futures_balances(&self) -> Result<Vec<BinanceFuturesBalance>, AccountError> {
        let client = self.futures_private_client()?;
        let response = client.get("/fapi/v2/balance").await?;
        let balances: Vec<BinanceFuturesBalance> = response
            .json()
            .await
            .map_err(|e| AccountError::Parse(e.to_string()))?;
        Ok(balances)
    }

    async fn fetch_instruments(&self) -> Result<Vec<Instrument>, MarketDataError> {
        let mut instruments: Vec<Instrument> = Vec::new();

        // Fetch Futures exchange info and funding info
        if let Ok(client) = self.futures_client() {
            debug!("Fetching Binance Futures exchange info");
            let response = client.get("/fapi/v1/exchangeInfo").await?;
            let info: BinanceFuturesExchangeInfo = response
                .json()
                .await
                .map_err(|e| MarketDataError::Parse(e.to_string()))?;

            // Fetch funding info to get funding interval per symbol
            debug!("Fetching Binance Futures funding info");
            let funding_response = client.get("/fapi/v1/fundingInfo").await?;
            let funding_info: Vec<BinanceFundingInfo> = funding_response
                .json()
                .await
                .map_err(|e| MarketDataError::Parse(e.to_string()))?;

            // Build a map of symbol -> funding_interval_hours
            let funding_map: HashMap<&str, u32> = funding_info
                .iter()
                .map(|f| (f.symbol.as_str(), f.funding_interval_hours))
                .collect();

            for symbol in &info.symbols {
                let funding_hours = funding_map.get(symbol.symbol.as_str()).copied();
                if let Some(meta) = BinanceSwapMeta::try_new(symbol, funding_hours)
                    && meta.is_derivative()
                    && meta.is_trading_allowed()
                {
                    instruments.push(meta.into());
                }
            }
            debug!("Parsed {} Futures instruments", instruments.len());
        }

        if let Ok(client) = self.spot_client() {
            let spot_count_before = instruments.len();
            debug!("Fetching Binance Spot exchange info");
            let response = client.get("/api/v3/exchangeInfo").await?;
            let info: BinanceSpotExchangeInfo = response
                .json()
                .await
                .map_err(|e| MarketDataError::Parse(e.to_string()))?;

            for symbol in &info.symbols {
                if let Some(meta) = BinanceSpotMeta::try_new(symbol)
                    && meta.is_trading_allowed()
                {
                    instruments.push(meta.into());
                }
            }
            debug!(
                "Parsed {} Spot instruments",
                instruments.len() - spot_count_before
            );
        }

        Ok(instruments)
    }

    fn futures_client(&self) -> Result<&TracedHttpClient, MarketDataError> {
        self.futures_public_client
            .as_ref()
            .ok_or(MarketDataError::UnsupportedInstrument)
    }

    fn spot_client(&self) -> Result<&TracedHttpClient, MarketDataError> {
        self.spot_public_client
            .as_ref()
            .ok_or(MarketDataError::UnsupportedInstrument)
    }

    fn futures_private_client(&self) -> Result<&TracedHttpClient, AccountError> {
        self.futures_private_client.as_ref().ok_or_else(|| {
            AccountError::Other("missing futures private client configuration".to_string())
        })
    }

    fn spot_private_client(&self) -> Result<&TracedHttpClient, AccountError> {
        self.spot_private_client.as_ref().ok_or_else(|| {
            AccountError::Other("missing spot private client configuration".to_string())
        })
    }
}

#[async_trait]
impl MarketDataProvider for BinanceClient {
    async fn load_instruments(&self) -> Result<(), MarketDataError> {
        self.instruments_loaded
            .get_or_try_init(|| async {
                info!("Loading Binance instruments...");
                let instruments = self.fetch_instruments().await?;
                self.registry.insert_batch(instruments);
                info!("Binance instruments loaded: {} total", self.registry.len());
                Ok::<(), MarketDataError>(())
            })
            .await
            .map(|_| ())
    }

    async fn health_check(&self) -> Result<(), MarketDataError> {
        // Check futures if configured
        if let Ok(client) = self.futures_client() {
            client.get("/fapi/v1/ping").await?;
            debug!("Binance Futures health check OK");
        }

        // Check spot if configured
        if let Ok(client) = self.spot_client() {
            client.get("/api/v3/ping").await?;
            debug!("Binance Spot health check OK");
        }

        Ok(())
    }

    async fn server_time(&self) -> Result<DateTime<Utc>, MarketDataError> {
        // Prefer futures time if available, fallback to spot
        let response = if let Ok(client) = self.futures_client() {
            client.get("/fapi/v1/time").await?
        } else if let Ok(client) = self.spot_client() {
            client.get("/api/v3/time").await?
        } else {
            return Err(MarketDataError::Other("No client configured".to_string()));
        };

        let time: BinanceServerTime = response
            .json()
            .await
            .map_err(|e| MarketDataError::Parse(e.to_string()))?;

        Ok(ms_to_datetime(time.server_time).ok_or_else(|| {
            MarketDataError::Parse("invalid server timestamp from Binance".to_string())
        })?)
    }
}

#[async_trait]
impl FundingRateMarketData for BinanceClient {
    async fn funding_rate_snapshot(
        &self,
        key: InstrumentKey,
    ) -> Result<FundingRateSnapshot, MarketDataError> {
        // Funding rate only for Swap/Futures
        if key.instrument_type != InstrumentType::Swap {
            return Err(MarketDataError::UnsupportedInstrument);
        }

        // Get funding interval from registry (caller should have loaded instruments)
        let instrument = self
            .registry
            .get(&key)
            .ok_or(MarketDataError::UnsupportedInstrument)?;
        let interval = instrument
            .funding_interval
            .unwrap_or(FundingInterval::Every8Hours);

        let client = self.futures_client()?;
        let symbol = instrument.exchange_symbol;
        let path = format!("/fapi/v1/premiumIndex?symbol={symbol}");

        debug!("Fetching premium index for {}", symbol);
        let response = client.get(&path).await?;
        let index: BinancePremiumIndex = response
            .json()
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
        // Funding rate only for Swap/Futures
        if key.instrument_type != InstrumentType::Swap {
            return Err(MarketDataError::UnsupportedInstrument);
        }

        // Get funding interval from registry (caller should have loaded instruments)
        let instrument = self
            .registry
            .get(&key)
            .ok_or(MarketDataError::UnsupportedInstrument)?;
        let interval = instrument
            .funding_interval
            .unwrap_or(FundingInterval::Every8Hours);

        let client = self.futures_client()?;
        let symbol = instrument.exchange_symbol;
        let start_ms = start.timestamp_millis();
        let end_ms = end.timestamp_millis();
        let limit = limit.min(1000); // Binance max is 1000

        let path = format!(
            "/fapi/v1/fundingRate?symbol={symbol}&startTime={start_ms}&endTime={end_ms}&limit={limit}",
        );

        debug!(
            "Fetching funding rate history for {} from {} to {}",
            symbol, start, end
        );
        let response = client.get(&path).await?;
        let history: Vec<BinanceFundingRateHistory> = response
            .json()
            .await
            .map_err(|e| MarketDataError::Parse(e.to_string()))?;

        Ok(funding_history_to_series(&history, key.pair, interval))
    }
}

#[async_trait]
impl Account for BinanceClient {
    async fn account_snapshot(&self) -> Result<AccountSnapshot, AccountError> {
        let as_of_utc = Utc::now();

        let include_spot = matches!(
            self.config.mode,
            BinanceApiMode::Spot | BinanceApiMode::Both
        );
        let include_futures = matches!(
            self.config.mode,
            BinanceApiMode::Futures | BinanceApiMode::Both
        );

        let spot_balances = if include_spot {
            self.fetch_spot_balances().await?
        } else {
            Vec::new()
        };

        let futures_balances = if include_futures {
            self.fetch_futures_balances().await?
        } else {
            Vec::new()
        };

        Ok(build_segmented_account_snapshot(
            include_spot,
            include_futures,
            &spot_balances,
            &futures_balances,
            as_of_utc,
        ))
    }
}

#[async_trait]
impl AccountSnapshotSource for BinanceClient {
    async fn fetch_account_snapshot(
        &self,
        exchange: Exchange,
    ) -> Result<AccountSnapshot, AccountManagerError> {
        if exchange != Exchange::Binance {
            return Err(AccountManagerError::Other {
                reason: format!("unsupported exchange for BinanceClient: {exchange:?}"),
            });
        }

        self.account_snapshot()
            .await
            .map_err(AccountManagerError::SourceError)
    }
}

#[async_trait]
impl OrderExecutionProvider for BinanceClient {
    fn format_client_id(&self, internal_id: &str) -> String {
        // Binance allows up to 36 characters for clientOrderId
        let mut id = internal_id.to_string();
        if id.len() > 36 {
            id.truncate(36);
        }
        id
    }

    async fn place_order(
        &self,
        _request: OrderRequest,
        _client_order_id: String,
    ) -> Result<Order, TradingError> {
        Err(TradingError::Other(
            "Binance place_order not implemented yet".to_string(),
        ))
    }

    async fn cancel_order(
        &self,
        _key: InstrumentKey,
        _order_id: &str,
    ) -> Result<Order, TradingError> {
        Err(TradingError::Other(
            "Binance cancel_order not implemented yet".to_string(),
        ))
    }

    async fn get_open_orders(&self, _key: InstrumentKey) -> Result<Vec<Order>, TradingError> {
        Err(TradingError::Other(
            "Binance get_open_orders not implemented yet".to_string(),
        ))
    }

    async fn get_order_status(
        &self,
        _key: InstrumentKey,
        _order_id: &str,
    ) -> Result<Order, TradingError> {
        Err(TradingError::Other(
            "Binance get_order_status not implemented yet".to_string(),
        ))
    }

    async fn stream_executions(&self, tx: mpsc::Sender<OrderEvent>) -> Result<(), AccountError> {
        let include_spot = matches!(
            self.config.mode,
            BinanceApiMode::Spot | BinanceApiMode::Both
        );
        let include_futures = matches!(
            self.config.mode,
            BinanceApiMode::Futures | BinanceApiMode::Both
        );

        let cancel = CancellationToken::new();

        // Cancel all supervisors when the receiver side of the channel is dropped.
        let cancel_on_close = cancel.clone();
        let tx_watcher = tx.clone();
        tokio::spawn(async move {
            tx_watcher.closed().await;
            cancel_on_close.cancel();
        });

        let mut handles = Vec::new();

        if include_spot {
            let (api_key, api_secret) = {
                let key = self.config.api_key.clone().ok_or_else(|| {
                    AccountError::Other("missing api_key for Spot WS stream".to_string())
                })?;
                let secret = self.config.api_secret.clone().ok_or_else(|| {
                    AccountError::Other("missing api_secret for Spot WS stream".to_string())
                })?;
                (key, secret)
            };

            let ws_api_url = BINANCE_SPOT_WS_API_URL.to_string();
            let policy = BinanceSpotUserDataPolicy::new(
                ws_api_url,
                api_key,
                api_secret,
                self.registry.clone(),
                tx.clone(),
            );
            handles.push(ws_supervisor_spawn(policy, cancel.clone()));
            info!("Binance Spot user-data supervisor spawned (WS API)");
        }

        if include_futures {
            let client = self.futures_private_client()?.clone();
            let base_ws_url = if self.config.testnet {
                BINANCE_FUTURES_WS_TESTNET_URL
            } else {
                BINANCE_FUTURES_WS_BASE_URL
            }
            .to_string();

            let policy = BinanceFuturesUserDataPolicy::new(
                client,
                base_ws_url,
                self.registry.clone(),
                tx.clone(),
            );
            handles.push(ws_supervisor_spawn(policy, cancel.clone()));
            info!("Binance Futures user-data supervisor spawned");
        }

        // Block until all supervisors exit (triggered by cancellation or fatal error).
        for handle in handles {
            handle.join().await;
        }

        Ok(())
    }
}

#[async_trait]
impl PositionProvider for BinanceClient {
    async fn fetch_positions(&self) -> Result<Vec<Position>, TradingError> {
        Err(TradingError::Other(
            "Binance fetch_positions not implemented yet".to_string(),
        ))
    }
}

#[async_trait]
impl OrderBookFeeder for BinanceClient {
    async fn fetch_order_book(&self, key: InstrumentKey) -> Result<OrderBookSnapshot, PriceError> {
        let instrument = self
            .registry
            .get(&key)
            .ok_or(PriceError::UnsupportedInstrument)?;
        let symbol = instrument.exchange_symbol;
        let timestamp_ms = Utc::now().timestamp_millis();

        debug!(key = ?key, symbol = %symbol, "Fetching REST order-book snapshot");

        match key.instrument_type {
            InstrumentType::Swap => {
                let client = self
                    .futures_client()
                    .map_err(|e| PriceError::Other(e.to_string()))?;
                let endpoint = format!("/fapi/v1/depth?symbol={symbol}&limit=5");
                let resp = client.get(&endpoint).await.map_err(PriceError::from)?;
                let depth: BinancePartialDepth = resp
                    .json()
                    .await
                    .map_err(|e| PriceError::Parse(e.to_string()))?;
                let snapshot = partial_depth_to_order_book(&depth, key, timestamp_ms)
                    .ok_or_else(|| PriceError::Parse("failed to parse depth levels".to_string()))?;
                debug!(
                    key = ?key,
                    best_bid = ?snapshot.best_bid,
                    best_ask = ?snapshot.best_ask,
                    spread = ?snapshot.spread,
                    "Futures REST order-book snapshot parsed"
                );
                Ok(snapshot)
            }
            InstrumentType::Spot => {
                let client = self
                    .spot_client()
                    .map_err(|e| PriceError::Other(e.to_string()))?;
                let endpoint = format!("/api/v3/depth?symbol={symbol}&limit=5");
                let resp = client.get(&endpoint).await.map_err(PriceError::from)?;
                let depth: BinancePartialDepth = resp
                    .json()
                    .await
                    .map_err(|e| PriceError::Parse(e.to_string()))?;
                let snapshot = partial_depth_to_order_book(&depth, key, timestamp_ms)
                    .ok_or_else(|| PriceError::Parse("failed to parse depth levels".to_string()))?;
                debug!(
                    key = ?key,
                    best_bid = ?snapshot.best_bid,
                    best_ask = ?snapshot.best_ask,
                    spread = ?snapshot.spread,
                    "Spot REST order-book snapshot parsed"
                );
                Ok(snapshot)
            }
        }
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

        let spot_keys: Vec<&InstrumentKey> = keys
            .iter()
            .filter(|k| k.instrument_type == InstrumentType::Spot)
            .collect();

        let cancel = CancellationToken::new();

        // Cancel all depth supervisors when the depth channel receiver is dropped.
        let cancel_on_close = cancel.clone();
        let tx_watcher = tx.clone();
        tokio::spawn(async move {
            tx_watcher.closed().await;
            cancel_on_close.cancel();
        });

        let mut handles = Vec::new();

        if !futures_keys.is_empty() {
            let mut map = HashMap::new();
            for k in &futures_keys {
                if let Some(instrument) = self.registry.get(k) {
                    let symbol_lower = instrument.exchange_symbol.to_lowercase();
                    map.insert(symbol_lower, **k);
                }
            }
            let base_url = if self.config.testnet {
                BINANCE_FUTURES_WS_TESTNET_URL
            } else {
                BINANCE_FUTURES_WS_BASE_URL
            }
            .to_string();

            let policy = BinanceDepthPolicy::new(base_url, map, tx.clone());
            handles.push(ws_supervisor_spawn(policy, cancel.clone()));
            info!("Binance Futures depth supervisor spawned");
        }

        if !spot_keys.is_empty() {
            let mut map = HashMap::new();
            for k in &spot_keys {
                if let Some(instrument) = self.registry.get(k) {
                    let symbol_lower = instrument.exchange_symbol.to_lowercase();
                    map.insert(symbol_lower, **k);
                }
            }
            let base_url = if self.config.testnet {
                BINANCE_SPOT_WS_TESTNET_URL
            } else {
                BINANCE_SPOT_WS_BASE_URL
            }
            .to_string();

            let policy = BinanceDepthPolicy::new(base_url, map, tx.clone());
            handles.push(ws_supervisor_spawn(policy, cancel.clone()));
            info!("Binance Spot depth supervisor spawned");
        }

        for handle in handles {
            handle.join().await;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{binance as fixtures, empty_registry, test_registry};
    use boldtrax_core::traits::Account;
    use boldtrax_core::types::{Exchange, Pairs};
    use rust_decimal_macros::dec;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_client(
        mock_server: &MockServer,
        mode: BinanceApiMode,
        registry: InstrumentRegistry,
    ) -> BinanceClient {
        let config = BinanceConfig {
            mode,
            timeout_secs: 5,
            max_retries: 0,
            api_key: None,
            api_secret: None,
            futures_base_url: Some(mock_server.uri()),
            spot_base_url: Some(mock_server.uri()),
            testnet: false,
        };
        BinanceClient::new(config, registry).expect("Failed to create test client")
    }

    #[tokio::test]
    async fn health_check_futures_only() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/fapi/v1/ping"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&mock_server)
            .await;

        let client = test_client(&mock_server, BinanceApiMode::Futures, empty_registry());
        let result = client.health_check().await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn health_check_spot_only() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v3/ping"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&mock_server)
            .await;

        let client = test_client(&mock_server, BinanceApiMode::Spot, empty_registry());
        let result = client.health_check().await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn server_time_returns_correct_datetime() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/fapi/v1/time"))
            .respond_with(ResponseTemplate::new(200).set_body_string(fixtures::server_time_json()))
            .mount(&mock_server)
            .await;

        let client = test_client(&mock_server, BinanceApiMode::Futures, empty_registry());
        let result = client.server_time().await;

        assert!(result.is_ok());
        let time = result.unwrap();
        assert_eq!(time.timestamp_millis(), 1_737_410_400_000);
    }

    #[tokio::test]
    async fn load_instruments_parses_futures_correctly() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/fapi/v1/exchangeInfo"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(fixtures::futures_exchange_info_json()),
            )
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/fapi/v1/fundingInfo"))
            .respond_with(ResponseTemplate::new(200).set_body_string(fixtures::funding_info_json()))
            .mount(&mock_server)
            .await;

        let registry = empty_registry();
        let client = test_client(&mock_server, BinanceApiMode::Futures, registry.clone());

        let result = client.load_instruments().await;
        assert!(result.is_ok());
        assert_eq!(registry.len(), 2);

        let btc_key = InstrumentKey {
            exchange: Exchange::Binance,
            instrument_type: InstrumentType::Swap,
            pair: Pairs::BTCUSDT,
        };
        let btc = registry.get(&btc_key).expect("BTCUSDT should exist");
        assert_eq!(btc.exchange_symbol, "BTCUSDT");
        assert_eq!(btc.tick_size, dec!(0.10));
        assert_eq!(btc.lot_size, dec!(0.001));
        assert_eq!(btc.min_notional, Some(dec!(5)));
        assert_eq!(btc.funding_interval, Some(FundingInterval::Every8Hours));
    }

    #[tokio::test]
    async fn load_instruments_parses_spot_correctly() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v3/exchangeInfo"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(fixtures::spot_exchange_info_json()),
            )
            .mount(&mock_server)
            .await;

        let registry = empty_registry();
        let client = test_client(&mock_server, BinanceApiMode::Spot, registry.clone());

        let result = client.load_instruments().await;
        assert!(result.is_ok());
        assert_eq!(registry.len(), 2);

        let btc_key = InstrumentKey {
            exchange: Exchange::Binance,
            instrument_type: InstrumentType::Spot,
            pair: Pairs::BTCUSDT,
        };
        let btc = registry.get(&btc_key).expect("BTCUSDT spot should exist");
        assert_eq!(btc.exchange_symbol, "BTCUSDT");
        assert_eq!(btc.funding_interval, None);
    }

    #[tokio::test]
    async fn load_instruments_skips_if_already_loaded() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/fapi/v1/exchangeInfo"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(fixtures::futures_exchange_info_json()),
            )
            .expect(1)
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/fapi/v1/fundingInfo"))
            .respond_with(ResponseTemplate::new(200).set_body_string(fixtures::funding_info_json()))
            .expect(1)
            .mount(&mock_server)
            .await;

        let registry = empty_registry();
        let client = test_client(&mock_server, BinanceApiMode::Futures, registry.clone());

        client.load_instruments().await.unwrap();
        assert_eq!(registry.len(), 2);

        client.load_instruments().await.unwrap();
    }

    #[tokio::test]
    async fn funding_rate_snapshot_parses_correctly() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/fapi/v1/premiumIndex"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(fixtures::premium_index_json()),
            )
            .mount(&mock_server)
            .await;

        let registry = test_registry();
        let client = test_client(&mock_server, BinanceApiMode::Futures, registry.clone());

        let key = InstrumentKey {
            exchange: Exchange::Binance,
            instrument_type: InstrumentType::Swap,
            pair: Pairs::BTCUSDT,
        };

        let result = client.funding_rate_snapshot(key).await;
        assert!(result.is_ok());

        let snapshot = result.unwrap();
        assert_eq!(snapshot.key.pair, Pairs::BTCUSDT);
        assert_eq!(snapshot.funding_rate, dec!(0.0001));
        assert_eq!(snapshot.mark_price, dec!(100000.50));
        assert_eq!(snapshot.index_price, dec!(100000.00));
    }

    #[tokio::test]
    async fn funding_rate_snapshot_rejects_spot() {
        let mock_server = MockServer::start().await;
        let registry = test_registry();
        let client = test_client(&mock_server, BinanceApiMode::Futures, registry.clone());

        let key = InstrumentKey {
            exchange: Exchange::Binance,
            instrument_type: InstrumentType::Spot,
            pair: Pairs::BTCUSDT,
        };

        let result = client.funding_rate_snapshot(key).await;
        assert!(matches!(
            result,
            Err(MarketDataError::UnsupportedInstrument)
        ));
    }

    #[tokio::test]
    async fn funding_rate_history_parses_correctly() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/fapi/v1/fundingRate"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(fixtures::funding_rate_history_json()),
            )
            .mount(&mock_server)
            .await;

        let registry = test_registry();
        let client = test_client(&mock_server, BinanceApiMode::Futures, registry.clone());

        let key = InstrumentKey {
            exchange: Exchange::Binance,
            instrument_type: InstrumentType::Swap,
            pair: Pairs::BTCUSDT,
        };

        let start = chrono::Utc::now() - chrono::Duration::days(1);
        let end = chrono::Utc::now();

        let result = client.funding_rate_history(key, start, end, 100).await;
        assert!(result.is_ok());

        let series = result.unwrap();
        assert_eq!(series.key.pair, Pairs::BTCUSDT);
        assert_eq!(series.points.len(), 3);
        assert_eq!(series.points[0].funding_rate, dec!(0.0001));
    }

    #[test]
    fn default_config_is_both_mode() {
        let config = BinanceConfig::default();
        assert_eq!(config.mode, BinanceApiMode::Both);
        assert_eq!(config.timeout_secs, 30);
        assert_eq!(config.max_retries, 3);
        assert!(config.api_key.is_none());
        assert!(config.api_secret.is_none());
    }

    #[test]
    fn futures_only_mode_has_no_spot_client() {
        let config = BinanceConfig {
            mode: BinanceApiMode::Futures,
            ..Default::default()
        };
        let client = BinanceClient::new(config, empty_registry()).unwrap();

        assert!(client.futures_client().is_ok());
        assert!(client.spot_client().is_err());
    }

    #[test]
    fn spot_only_mode_has_no_futures_client() {
        let config = BinanceConfig {
            mode: BinanceApiMode::Spot,
            ..Default::default()
        };
        let client = BinanceClient::new(config, empty_registry()).unwrap();

        assert!(client.futures_client().is_err());
        assert!(client.spot_client().is_ok());
    }

    #[tokio::test]
    async fn account_snapshot_requires_private_config() {
        let mock_server = MockServer::start().await;
        let client = test_client(&mock_server, BinanceApiMode::Both, empty_registry());

        let result = client.account_snapshot().await;
        assert!(matches!(result, Err(AccountError::Other(_))));
    }

    #[tokio::test]
    async fn account_snapshot_pulls_spot_and_futures_with_private_auth() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v3/account"))
            .and(header("x-mbx-apikey", "test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_string(fixtures::spot_account_json()))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/fapi/v2/balance"))
            .and(header("x-mbx-apikey", "test-key"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(fixtures::futures_balance_json()),
            )
            .mount(&mock_server)
            .await;

        let config = BinanceConfig {
            mode: BinanceApiMode::Both,
            timeout_secs: 5,
            max_retries: 0,
            api_key: Some("test-key".to_string()),
            api_secret: Some("test-secret".to_string()),
            futures_base_url: Some(mock_server.uri()),
            spot_base_url: Some(mock_server.uri()),
            testnet: false,
        };
        let client =
            BinanceClient::new(config, empty_registry()).expect("client should be created");

        let snapshot = client
            .account_snapshot()
            .await
            .expect("account snapshot should be fetched");

        assert_eq!(snapshot.exchange, Exchange::Binance);
        assert_eq!(
            snapshot.model,
            boldtrax_core::manager::types::AccountModel::Segmented
        );
        assert_eq!(snapshot.partitions.len(), 2);

        let usdt_spot = snapshot
            .balances
            .iter()
            .find(|b| b.asset == boldtrax_core::types::Currency::USDT && b.total == dec!(110.5))
            .expect("spot USDT balance should exist");
        assert_eq!(usdt_spot.free, dec!(100.5));
        assert_eq!(usdt_spot.locked, dec!(10));

        let usdt_futures = snapshot
            .balances
            .iter()
            .find(|b| b.asset == boldtrax_core::types::Currency::USDT && b.total == dec!(250))
            .expect("futures USDT balance should exist");
        assert_eq!(usdt_futures.unrealized_pnl, dec!(5.25));
    }
}
