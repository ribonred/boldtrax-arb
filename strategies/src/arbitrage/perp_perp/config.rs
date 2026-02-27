use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;

use boldtrax_core::types::{Exchange, InstrumentKey};
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::arbitrage::margin::MarginManager;
use crate::arbitrage::perp_perp::decider::PerpPerpDecider;
use crate::arbitrage::perp_perp::types::CarryDirection;

/// Deserialized from `[strategy.perp_perp]` in the TOML config stack.
/// All fields are mandatory unless noted — the strategy panics at startup
/// if any are missing so we catch misconfigurations before risking capital.
#[derive(Debug, Clone, Deserialize)]
pub struct PerpPerpStrategyConfig {
    /// Carry direction: `"positive"` (long receives funding) or `"negative"`.
    pub carry_direction: CarryDirection,
    /// Minimum `|long_rate - short_rate|` to justify entering.
    pub min_spread_threshold: Decimal,
    /// Optional: exit when spread drops below this value.
    /// If omitted, exit immediately when spread flips sign (< 0).
    pub exit_threshold: Option<Decimal>,
    /// Target dollar exposure per side.
    pub target_notional: Decimal,
    /// Delta drift threshold to trigger rebalance (% of target_notional).
    pub rebalance_drift_pct: Decimal,
    /// Minimum distance from liquidation price as % of mark price.
    pub liquidation_buffer_pct: Decimal,
    /// Full InstrumentKey for the long leg, e.g. `"XRPUSDT-BN-SWAP"`.
    pub perp_long_instrument: String,
    /// Full InstrumentKey for the short leg, e.g. `"XRPUSDT-BY-SWAP"`.
    pub perp_short_instrument: String,
    /// How often to poll funding rates (seconds). Default: 30.
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    /// Cache staleness threshold (seconds). Default: 60.
    #[serde(default = "default_cache_staleness")]
    pub cache_staleness_secs: u64,
}

fn default_poll_interval() -> u64 {
    30
}

fn default_cache_staleness() -> u64 {
    60
}

impl PerpPerpStrategyConfig {
    pub fn from_strategy_map(strategy: &HashMap<String, toml::Value>) -> Self {
        let section = strategy
            .get("perp_perp")
            .expect("[strategy.perp_perp] section is required");

        let config: Self = section
            .clone()
            .try_into()
            .expect("[strategy.perp_perp] contains invalid or missing fields");

        config.validate();
        config
    }

    fn validate(&self) {
        assert!(
            self.min_spread_threshold > Decimal::ZERO,
            "strategy.perp_perp.min_spread_threshold must be > 0"
        );
        assert!(
            self.target_notional > Decimal::ZERO,
            "strategy.perp_perp.target_notional must be > 0"
        );
        assert!(
            self.rebalance_drift_pct > Decimal::ZERO,
            "strategy.perp_perp.rebalance_drift_pct must be > 0"
        );
        assert!(
            self.liquidation_buffer_pct > Decimal::ZERO,
            "strategy.perp_perp.liquidation_buffer_pct must be > 0"
        );
        assert!(
            self.poll_interval_secs > 0,
            "strategy.perp_perp.poll_interval_secs must be > 0"
        );
        assert!(
            self.cache_staleness_secs > 0,
            "strategy.perp_perp.cache_staleness_secs must be > 0"
        );
        InstrumentKey::from_str(&self.perp_long_instrument).unwrap_or_else(|_| {
            panic!(
                "strategy.perp_perp.perp_long_instrument '{}' is not a valid InstrumentKey",
                self.perp_long_instrument
            )
        });
        InstrumentKey::from_str(&self.perp_short_instrument).unwrap_or_else(|_| {
            panic!(
                "strategy.perp_perp.perp_short_instrument '{}' is not a valid InstrumentKey",
                self.perp_short_instrument
            )
        });
    }

    pub fn long_key(&self) -> InstrumentKey {
        InstrumentKey::from_str(&self.perp_long_instrument).expect("validated")
    }

    pub fn short_key(&self) -> InstrumentKey {
        InstrumentKey::from_str(&self.perp_short_instrument).expect("validated")
    }

    pub fn exchange_long(&self) -> Exchange {
        self.long_key().exchange
    }

    pub fn exchange_short(&self) -> Exchange {
        self.short_key().exchange
    }

    pub fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.poll_interval_secs)
    }

    pub fn cache_staleness(&self) -> Duration {
        Duration::from_secs(self.cache_staleness_secs)
    }

    pub fn build_decider(&self) -> PerpPerpDecider {
        PerpPerpDecider::new(
            self.min_spread_threshold,
            self.exit_threshold,
            self.target_notional,
            self.rebalance_drift_pct,
        )
    }

    pub fn build_margin(&self, max_leverage: Decimal) -> MarginManager {
        MarginManager::new(max_leverage, self.liquidation_buffer_pct)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn valid_strategy_map() -> HashMap<String, toml::Value> {
        let toml_str = r#"
            carry_direction = "positive"
            min_spread_threshold = "0.0002"
            target_notional = "500"
            rebalance_drift_pct = "10"
            liquidation_buffer_pct = "15"
            perp_long_instrument = "XRPUSDT-BN-SWAP"
            perp_short_instrument = "XRPUSDT-BY-SWAP"
            poll_interval_secs = 30
            cache_staleness_secs = 60
        "#;
        let val: toml::Value = toml_str.parse().unwrap();
        let mut map = HashMap::new();
        map.insert("perp_perp".to_string(), val);
        map
    }

    #[test]
    fn parses_valid_config() {
        let map = valid_strategy_map();
        let cfg = PerpPerpStrategyConfig::from_strategy_map(&map);
        assert_eq!(cfg.carry_direction, CarryDirection::Positive);
        assert_eq!(cfg.min_spread_threshold, dec!(0.0002));
        assert_eq!(cfg.target_notional, dec!(500));
        assert_eq!(cfg.rebalance_drift_pct, dec!(10));
        assert_eq!(cfg.liquidation_buffer_pct, dec!(15));
        assert_eq!(cfg.poll_interval_secs, 30);
        assert_eq!(cfg.cache_staleness_secs, 60);
        assert!(cfg.exit_threshold.is_none());
    }

    #[test]
    fn parses_config_with_exit_threshold() {
        let toml_str = r#"
            carry_direction = "negative"
            min_spread_threshold = "0.0003"
            exit_threshold = "-0.0001"
            target_notional = "200"
            rebalance_drift_pct = "5"
            liquidation_buffer_pct = "10"
            perp_long_instrument = "BTCUSDT-BN-SWAP"
            perp_short_instrument = "BTCUSDT-BY-SWAP"
        "#;
        let val: toml::Value = toml_str.parse().unwrap();
        let mut map = HashMap::new();
        map.insert("perp_perp".to_string(), val);

        let cfg = PerpPerpStrategyConfig::from_strategy_map(&map);
        assert_eq!(cfg.carry_direction, CarryDirection::Negative);
        assert_eq!(cfg.exit_threshold, Some(dec!(-0.0001)));
        // Defaults for omitted fields
        assert_eq!(cfg.poll_interval_secs, 30);
        assert_eq!(cfg.cache_staleness_secs, 60);
    }

    #[test]
    fn build_decider_uses_config_values() {
        let map = valid_strategy_map();
        let cfg = PerpPerpStrategyConfig::from_strategy_map(&map);
        let decider = cfg.build_decider();
        assert_eq!(decider.min_spread_threshold, dec!(0.0002));
        assert_eq!(decider.target_notional, dec!(500));
        assert_eq!(decider.rebalance_drift_pct, dec!(10));
        assert!(decider.exit_threshold.is_none());
    }

    #[test]
    fn build_margin_uses_config_and_risk() {
        let map = valid_strategy_map();
        let cfg = PerpPerpStrategyConfig::from_strategy_map(&map);
        let margin = cfg.build_margin(dec!(5));
        assert_eq!(margin.max_leverage, dec!(5));
        assert_eq!(margin.liquidation_buffer_pct, dec!(15));
    }

    #[test]
    fn derives_exchanges_from_keys() {
        let map = valid_strategy_map();
        let cfg = PerpPerpStrategyConfig::from_strategy_map(&map);
        assert_eq!(cfg.exchange_long(), Exchange::Binance);
        assert_eq!(cfg.exchange_short(), Exchange::Bybit);
    }

    #[test]
    #[should_panic(expected = "[strategy.perp_perp] section is required")]
    fn panics_on_missing_section() {
        let map = HashMap::new();
        PerpPerpStrategyConfig::from_strategy_map(&map);
    }

    #[test]
    #[should_panic(expected = "min_spread_threshold must be > 0")]
    fn panics_on_zero_threshold() {
        let toml_str = r#"
            carry_direction = "positive"
            min_spread_threshold = "0"
            target_notional = "500"
            rebalance_drift_pct = "10"
            liquidation_buffer_pct = "15"
            perp_long_instrument = "XRPUSDT-BN-SWAP"
            perp_short_instrument = "XRPUSDT-BY-SWAP"
        "#;
        let val: toml::Value = toml_str.parse().unwrap();
        let mut map = HashMap::new();
        map.insert("perp_perp".to_string(), val);
        PerpPerpStrategyConfig::from_strategy_map(&map);
    }
}
