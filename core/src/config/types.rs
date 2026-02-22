use crate::types::ExecutionMode;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::str::FromStr;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskConfig {
    pub max_position_size: Decimal,
    pub max_total_exposure: Decimal,
    pub max_leverage: Decimal,
    pub delta_threshold: Decimal,
    pub min_funding_rate: Decimal,
    pub max_negative_funding_rate: Decimal,
    pub daily_drawdown_limit_pct: Decimal,
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
        }
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub execution_mode: ExecutionMode,
    pub redis_url: String,
    pub update_interval_ms: u64,
    pub risk: RiskConfig,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub exchanges: HashMap<String, toml::Value>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            execution_mode: ExecutionMode::Paper,
            redis_url: "redis://127.0.0.1:6379".to_string(),
            update_interval_ms: 1000,
            risk: RiskConfig::default(),
            exchanges: HashMap::new(),
        }
    }
}

impl AppConfig {
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();

        if self.risk.max_leverage <= Decimal::ZERO {
            errors.push("Risk: max_leverage must be greater than 0".to_string());
        }
        if self.risk.max_position_size <= Decimal::ZERO {
            errors.push("Risk: max_position_size must be greater than 0".to_string());
        }
        if self.risk.max_total_exposure <= Decimal::ZERO {
            errors.push("Risk: max_total_exposure must be greater than 0".to_string());
        }
        if self.risk.daily_drawdown_limit_pct <= Decimal::ZERO {
            errors.push("Risk: daily_drawdown_limit_pct must be greater than 0".to_string());
        }

        if self.execution_mode == ExecutionMode::Live {
            for (name, config) in &self.exchanges {
                let api_key = config.get("api_key").and_then(|v: &toml::Value| v.as_str());
                let api_secret = config
                    .get("api_secret")
                    .and_then(|v: &toml::Value| v.as_str());

                if api_key.is_none() || api_key.unwrap().is_empty() {
                    errors.push(format!(
                        "Exchange {}: api_key is required for Live execution mode",
                        name
                    ));
                }
                if api_secret.is_none() || api_secret.unwrap().is_empty() {
                    errors.push(format!(
                        "Exchange {}: api_secret is required for Live execution mode",
                        name
                    ));
                }
            }
        }

        errors
    }
}
