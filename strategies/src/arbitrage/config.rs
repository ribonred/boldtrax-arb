use std::str::FromStr;

use boldtrax_core::types::{Exchange, InstrumentKey};
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::arbitrage::decider::SpotRebalanceDecider;
use crate::arbitrage::margin::MarginManager;

/// Deserialized from `[strategy.spot_perp]` in the TOML config stack.
/// All fields are mandatory — the strategy panics at startup if any are
/// missing so we catch misconfigurations before risking capital.
#[derive(Debug, Clone, Deserialize)]
pub struct SpotPerpStrategyConfig {
    /// Minimum perp funding rate to justify entering (e.g. 0.0001 = 0.01%).
    pub min_funding_threshold: Decimal,
    /// Target dollar exposure per side (spot buy + perp short).
    pub target_notional: Decimal,
    /// Maximum position size per symbol (USD).
    pub max_position_size: Decimal,
    /// Delta drift threshold to trigger rebalance (% of target_notional).
    pub rebalance_drift_pct: Decimal,
    /// Minimum distance from liquidation price as % of mark price.
    pub liquidation_buffer_pct: Decimal,
    /// Full InstrumentKey for the spot leg, e.g. `"SOLUSDT-BN-SPOT"`.
    pub spot_instrument: String,
    /// Full InstrumentKey for the perp leg, e.g. `"SOLUSDT-BN-SWAP"`.
    pub perp_instrument: String,
}

impl SpotPerpStrategyConfig {
    /// Parse from the `strategy` HashMap inside `AppConfig`.
    ///
    /// Panics if `[strategy.spot_perp]` is missing or contains invalid values
    /// — this is a critical config that must be present to run the strategy.
    pub fn from_strategy_map(strategy: &std::collections::HashMap<String, toml::Value>) -> Self {
        let section = strategy
            .get("spot_perp")
            .expect("[strategy.spot_perp] section is required");

        let config: Self = section
            .clone()
            .try_into()
            .expect("[strategy.spot_perp] contains invalid or missing fields");

        config.validate();
        config
    }

    /// Panic on invalid parameter values.
    fn validate(&self) {
        assert!(
            self.min_funding_threshold > Decimal::ZERO,
            "strategy.spot_perp.min_funding_threshold must be > 0"
        );
        assert!(
            self.target_notional > Decimal::ZERO,
            "strategy.spot_perp.target_notional must be > 0"
        );
        assert!(
            self.max_position_size > Decimal::ZERO,
            "strategy.spot_perp.max_position_size must be > 0"
        );
        assert!(
            self.rebalance_drift_pct > Decimal::ZERO,
            "strategy.spot_perp.rebalance_drift_pct must be > 0"
        );
        assert!(
            self.liquidation_buffer_pct > Decimal::ZERO,
            "strategy.spot_perp.liquidation_buffer_pct must be > 0"
        );
        InstrumentKey::from_str(&self.spot_instrument).unwrap_or_else(|_| {
            panic!(
                "strategy.spot_perp.spot_instrument '{}' is not a valid InstrumentKey",
                self.spot_instrument
            )
        });
        InstrumentKey::from_str(&self.perp_instrument).unwrap_or_else(|_| {
            panic!(
                "strategy.spot_perp.perp_instrument '{}' is not a valid InstrumentKey",
                self.perp_instrument
            )
        });
    }

    /// Parse the spot instrument field into an `InstrumentKey`.
    pub fn spot_key(&self) -> InstrumentKey {
        InstrumentKey::from_str(&self.spot_instrument).expect("validated")
    }

    /// Parse the perp instrument field into an `InstrumentKey`.
    pub fn perp_key(&self) -> InstrumentKey {
        InstrumentKey::from_str(&self.perp_instrument).expect("validated")
    }

    /// Derive the exchange from the spot instrument key.
    pub fn exchange(&self) -> Exchange {
        self.spot_key().exchange
    }

    /// Build the decision policy from this config.
    pub fn build_decider(&self) -> SpotRebalanceDecider {
        SpotRebalanceDecider::new(
            self.min_funding_threshold,
            self.max_position_size,
            self.target_notional,
            self.rebalance_drift_pct,
        )
    }

    /// Build the margin policy from this config + the global `max_leverage`
    /// from `RiskConfig`.
    pub fn build_margin(&self, max_leverage: Decimal) -> MarginManager {
        MarginManager::new(max_leverage, self.liquidation_buffer_pct)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use std::collections::HashMap;

    fn valid_strategy_map() -> HashMap<String, toml::Value> {
        let toml_str = r#"
            min_funding_threshold = "0.0001"
            target_notional = "1000"
            max_position_size = "1000"
            rebalance_drift_pct = "10"
            liquidation_buffer_pct = "5"
            spot_instrument = "SOLUSDT-BN-SPOT"
            perp_instrument = "SOLUSDT-BN-SWAP"
        "#;
        let val: toml::Value = toml_str.parse().unwrap();
        let mut map = HashMap::new();
        map.insert("spot_perp".to_string(), val);
        map
    }

    #[test]
    fn parses_valid_config() {
        let map = valid_strategy_map();
        let cfg = SpotPerpStrategyConfig::from_strategy_map(&map);
        assert_eq!(cfg.min_funding_threshold, dec!(0.0001));
        assert_eq!(cfg.target_notional, dec!(1000));
        assert_eq!(cfg.max_position_size, dec!(1000));
        assert_eq!(cfg.rebalance_drift_pct, dec!(10));
        assert_eq!(cfg.liquidation_buffer_pct, dec!(5));
    }

    #[test]
    fn build_decider_uses_config_values() {
        let map = valid_strategy_map();
        let cfg = SpotPerpStrategyConfig::from_strategy_map(&map);
        let decider = cfg.build_decider();
        assert_eq!(decider.min_funding_threshold, dec!(0.0001));
        assert_eq!(decider.max_position_size, dec!(1000));
        assert_eq!(decider.target_notional, dec!(1000));
        assert_eq!(decider.rebalance_drift_pct, dec!(10));
    }

    #[test]
    fn build_margin_uses_config_and_risk() {
        let map = valid_strategy_map();
        let cfg = SpotPerpStrategyConfig::from_strategy_map(&map);
        let margin = cfg.build_margin(dec!(3));
        assert_eq!(margin.max_leverage, dec!(3));
        assert_eq!(margin.liquidation_buffer_pct, dec!(5));
    }

    #[test]
    #[should_panic(expected = "[strategy.spot_perp] section is required")]
    fn panics_on_missing_section() {
        let map = HashMap::new();
        SpotPerpStrategyConfig::from_strategy_map(&map);
    }

    #[test]
    #[should_panic(expected = "min_funding_threshold must be > 0")]
    fn panics_on_zero_threshold() {
        let toml_str = r#"
            min_funding_threshold = "0"
            target_notional = "1000"
            max_position_size = "1000"
            rebalance_drift_pct = "10"
            liquidation_buffer_pct = "5"
            spot_instrument = "SOLUSDT-BN-SPOT"
            perp_instrument = "SOLUSDT-BN-SWAP"
        "#;
        let val: toml::Value = toml_str.parse().unwrap();
        let mut map = HashMap::new();
        map.insert("spot_perp".to_string(), val);
        SpotPerpStrategyConfig::from_strategy_map(&map);
    }
}
