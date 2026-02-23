use crate::types::{Exchange, ExecutionMode, InstrumentKey, InstrumentType, Pairs};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;

// ── Fat-finger guard ──────────────────────────────────────────────────────────

/// Pre-trade safety limits applied by the `OrderManagerActor` to reject
/// obviously wrong order requests before they reach the exchange.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FatFingerConfig {
    /// Maximum allowed single order size in base asset units.
    pub max_order_size: Decimal,
    /// Maximum allowed single order notional value in USD.
    pub max_order_notional_usd: Decimal,
    /// Maximum allowed price deviation from current market mid-price (%).
    pub max_price_deviation_pct: Decimal,
}

impl Default for FatFingerConfig {
    fn default() -> Self {
        Self {
            max_order_size: Decimal::new(10, 0),
            max_order_notional_usd: Decimal::new(100_000, 0),
            max_price_deviation_pct: Decimal::new(5, 0),
        }
    }
}

// ── Risk ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskConfig {
    pub max_position_size: Decimal,
    pub max_total_exposure: Decimal,
    pub max_leverage: Decimal,
    pub delta_threshold: Decimal,
    pub min_funding_rate: Decimal,
    pub max_negative_funding_rate: Decimal,
    pub daily_drawdown_limit_pct: Decimal,
    #[serde(default)]
    pub fat_finger: FatFingerConfig,
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            max_position_size: Decimal::from(1000),
            max_total_exposure: Decimal::from(5000),
            max_leverage: Decimal::from(1),
            delta_threshold: Decimal::from_str("0.05").unwrap(),
            min_funding_rate: Decimal::from_str("0.0001").unwrap(),
            max_negative_funding_rate: Decimal::from_str("-0.001").unwrap(),
            daily_drawdown_limit_pct: Decimal::from(5),
            fat_finger: FatFingerConfig::default(),
        }
    }
}

// ── Serde bridge for strum enums ──────────────────────────────────────────────

/// Serialize/deserialize any `Display + FromStr` type as a string.
/// Lets us use `Pairs` and `InstrumentType` directly in TOML structs
/// without adding serde derives to the core types.
mod str_serde {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::fmt::Display;
    use std::str::FromStr;

    pub fn serialize<T: Display, S: Serializer>(val: &T, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&val.to_string())
    }

    pub fn deserialize<'de, T, D>(d: D) -> Result<T, D::Error>
    where
        T: FromStr,
        T::Err: Display,
        D: Deserializer<'de>,
    {
        let s = String::deserialize(d)?;
        s.parse::<T>().map_err(serde::de::Error::custom)
    }
}

// ── Instrument reference (TOML-friendly) ──────────────────────────────────────

/// Lightweight reference to an instrument that can be deserialized from TOML.
/// Lives inside per-exchange config: `[[exchanges.binance.instruments]]`.
///
/// Uses the native `Pairs` and `InstrumentType` enums directly — values must
/// match our universe naming (e.g. `"SOLUSDT"`, `"swap"`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstrumentRef {
    #[serde(with = "str_serde")]
    pub pair: Pairs,
    #[serde(rename = "type", with = "str_serde")]
    pub instrument_type: InstrumentType,
}

impl InstrumentRef {
    /// Convert into a typed `InstrumentKey` for the given exchange.
    /// Infallible — parsing already happened at deserialization time.
    pub fn to_instrument_key(&self, exchange: Exchange) -> InstrumentKey {
        InstrumentKey {
            exchange,
            pair: self.pair,
            instrument_type: self.instrument_type,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountManagerCfg {
    #[serde(default = "default_64")]
    pub mailbox_capacity: usize,
    #[serde(default = "default_30")]
    pub snapshot_max_age_secs: u64,
}

impl Default for AccountManagerCfg {
    fn default() -> Self {
        Self {
            mailbox_capacity: 64,
            snapshot_max_age_secs: 30,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceManagerCfg {
    #[serde(default = "default_64")]
    pub mailbox_capacity: usize,
    #[serde(default = "default_256")]
    pub broadcast_capacity: usize,
    #[serde(default = "default_256")]
    pub ws_capacity: usize,
    #[serde(default = "default_10000")]
    pub max_cache_entries: u64,
    #[serde(default = "default_60")]
    pub ttl_secs: u64,
}

impl Default for PriceManagerCfg {
    fn default() -> Self {
        Self {
            mailbox_capacity: 64,
            broadcast_capacity: 256,
            ws_capacity: 256,
            max_cache_entries: 10_000,
            ttl_secs: 60,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderManagerCfg {
    #[serde(default = "default_128")]
    pub mailbox_capacity: usize,
    #[serde(default = "default_1024")]
    pub broadcast_capacity: usize,
    #[serde(default = "default_1024")]
    pub ws_capacity: usize,
    #[serde(default = "default_30")]
    pub reconcile_interval_secs: u64,
}

impl Default for OrderManagerCfg {
    fn default() -> Self {
        Self {
            mailbox_capacity: 128,
            broadcast_capacity: 1024,
            ws_capacity: 1024,
            reconcile_interval_secs: 30,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionManagerCfg {
    #[serde(default = "default_1024")]
    pub mailbox_capacity: usize,
    #[serde(default = "default_1024")]
    pub broadcast_capacity: usize,
    #[serde(default = "default_30")]
    pub reconcile_interval_secs: u64,
}

impl Default for PositionManagerCfg {
    fn default() -> Self {
        Self {
            mailbox_capacity: 1024,
            broadcast_capacity: 1024,
            reconcile_interval_secs: 30,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ManagersConfig {
    #[serde(default)]
    pub account: AccountManagerCfg,
    #[serde(default)]
    pub price: PriceManagerCfg,
    #[serde(default)]
    pub order: OrderManagerCfg,
    #[serde(default)]
    pub position: PositionManagerCfg,
}

/// Which strategy to run.
/// Deserialized from `[runner.strategies]` in the TOML config stack.
/// Instrument details live in the strategy-specific config (e.g. `[strategy.spot_perp]`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerStrategyConfig {
    /// Strategy variant name, e.g. `"SpotPerp"`.
    pub strategy: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerConfig {
    #[serde(default = "default_30")]
    pub account_reconcile_interval_secs: u64,
    #[serde(default = "default_60")]
    pub funding_rate_poll_interval_secs: u64,
    #[serde(default = "default_30")]
    pub zmq_ttl_secs: u64,
    #[serde(default)]
    pub managers: ManagersConfig,
    /// List of exchange names to start exchange runners for.
    /// Each name must have a matching `[exchanges.<name>]` config section.
    #[serde(default)]
    pub exchanges: Vec<String>,
    /// Strategy to run when using `--mode all` or `--mode strategy`.
    #[serde(default)]
    pub strategies: Option<RunnerStrategyConfig>,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            account_reconcile_interval_secs: 30,
            funding_rate_poll_interval_secs: 60,
            zmq_ttl_secs: 30,
            managers: ManagersConfig::default(),
            exchanges: vec![],
            strategies: None,
        }
    }
}

impl RunnerConfig {
    pub fn account_reconcile_interval(&self) -> Duration {
        Duration::from_secs(self.account_reconcile_interval_secs)
    }

    pub fn funding_rate_poll_interval(&self) -> Duration {
        Duration::from_secs(self.funding_rate_poll_interval_secs)
    }
}

pub trait ExchangeConfig: serde::de::DeserializeOwned + Default {
    const EXCHANGE_NAME: &'static str;

    fn from_app_config(app_config: &AppConfig) -> Result<Self, anyhow::Error> {
        if let Some(val) = app_config.exchanges.get(Self::EXCHANGE_NAME) {
            let config: Self = val.clone().try_into()?;
            Ok(config)
        } else {
            Ok(Self::default())
        }
    }

    /// Exchange-specific credential and config validation.
    /// Each exchange knows its own required fields (api_key, api_secret,
    /// passphrase for OKX, etc.). Called by the runner at startup.
    /// Panics if critical credentials are missing.
    fn validate(&self, execution_mode: ExecutionMode) {
        let _ = execution_mode;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub execution_mode: ExecutionMode,
    pub redis_url: String,
    #[serde(default)]
    pub runner: RunnerConfig,
    pub risk: RiskConfig,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub exchanges: HashMap<String, toml::Value>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub strategy: HashMap<String, toml::Value>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            execution_mode: ExecutionMode::Paper,
            redis_url: "redis://127.0.0.1:6379".to_string(),
            runner: RunnerConfig::default(),
            risk: RiskConfig::default(),
            exchanges: HashMap::new(),
            strategy: HashMap::new(),
        }
    }
}

impl AppConfig {
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();

        if self.risk.max_leverage <= Decimal::ZERO {
            errors.push("risk.max_leverage must be > 0".to_string());
        }
        if self.risk.max_position_size <= Decimal::ZERO {
            errors.push("risk.max_position_size must be > 0".to_string());
        }
        if self.risk.max_total_exposure <= Decimal::ZERO {
            errors.push("risk.max_total_exposure must be > 0".to_string());
        }
        if self.risk.daily_drawdown_limit_pct <= Decimal::ZERO {
            errors.push("risk.daily_drawdown_limit_pct must be > 0".to_string());
        }
        if self.risk.delta_threshold <= Decimal::ZERO {
            errors.push("risk.delta_threshold must be > 0".to_string());
        }
        if self.risk.min_funding_rate <= Decimal::ZERO {
            errors.push("risk.min_funding_rate must be > 0".to_string());
        }
        if self.risk.fat_finger.max_order_size <= Decimal::ZERO {
            errors.push("risk.fat_finger.max_order_size must be > 0".to_string());
        }
        if self.risk.fat_finger.max_order_notional_usd <= Decimal::ZERO {
            errors.push("risk.fat_finger.max_order_notional_usd must be > 0".to_string());
        }
        if self.risk.fat_finger.max_price_deviation_pct <= Decimal::ZERO {
            errors.push("risk.fat_finger.max_price_deviation_pct must be > 0".to_string());
        }

        if self.runner.funding_rate_poll_interval_secs == 0 {
            errors.push("runner.funding_rate_poll_interval_secs must be > 0".to_string());
        }
        if self.runner.account_reconcile_interval_secs == 0 {
            errors.push("runner.account_reconcile_interval_secs must be > 0".to_string());
        }

        // ── Live-mode: at least one exchange must be configured ────────
        if self.execution_mode == ExecutionMode::Live && self.exchanges.is_empty() {
            errors.push("No exchanges configured for Live mode".to_string());
        }

        for name in &self.runner.exchanges {
            if !self.exchanges.contains_key(name) {
                errors.push(format!(
                    "runner.exchanges lists '{}' but no [exchanges.{}] config found",
                    name, name
                ));
            }
        }

        errors
    }

    pub fn tracked_instruments(&self, exchange: Exchange) -> Vec<InstrumentKey> {
        let exchange_name = exchange.to_string();
        let Some(exchange_val) = self.exchanges.get(&exchange_name) else {
            return vec![];
        };
        let Some(instruments) = exchange_val.get("instruments") else {
            return vec![];
        };
        let Some(arr) = instruments.as_array() else {
            return vec![];
        };

        arr.iter()
            .filter_map(|v| v.as_str()?.parse::<InstrumentKey>().ok())
            .collect()
    }
}

fn default_30() -> u64 {
    30
}
fn default_60() -> u64 {
    60
}
fn default_64() -> usize {
    64
}
fn default_128() -> usize {
    128
}
fn default_256() -> usize {
    256
}
fn default_1024() -> usize {
    1024
}
fn default_10000() -> u64 {
    10_000
}
