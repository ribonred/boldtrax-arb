use std::fmt;
use std::str::FromStr;

use ::strategies::arbitrage::runner::StrategyKind;
use rust_decimal::Decimal;

use super::{StrategyWizard, prompt_decimal, prompt_instrument_key};

pub struct SpotPerpWizard;

impl fmt::Display for SpotPerpWizard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SpotPerp")
    }
}

impl StrategyWizard for SpotPerpWizard {
    fn kind(&self) -> StrategyKind {
        StrategyKind::SpotPerp
    }

    fn prompt(&self) -> anyhow::Result<toml::Value> {
        println!("\n--- SpotPerp Strategy Configuration ---\n");

        println!("Spot leg instrument (must be SPOT type):");
        let spot_key = loop {
            let key = prompt_instrument_key("Spot instrument key:", "SOLUSDT-BN-SPOT")?;
            if key.instrument_type != boldtrax_core::types::InstrumentType::Spot {
                println!(
                    "  [WARN] Expected SPOT type, got {:?}. Try again.",
                    key.instrument_type
                );
                continue;
            }
            break key;
        };

        println!("Perp leg instrument (must be SWAP type):");
        let perp_key = loop {
            let key = prompt_instrument_key("Perp instrument key:", "SOLUSDT-BN-SWAP")?;
            if key.instrument_type != boldtrax_core::types::InstrumentType::Swap {
                println!(
                    "  [WARN] Expected SWAP type, got {:?}. Try again.",
                    key.instrument_type
                );
                continue;
            }
            break key;
        };

        let min_funding_threshold = prompt_decimal(
            "Minimum funding rate to enter (e.g. 0.00003):",
            Decimal::from_str("0.00003").unwrap(),
        )?;

        let target_notional =
            prompt_decimal("Target dollar exposure per side (USD):", Decimal::from(280))?;

        let rebalance_drift_pct = prompt_decimal(
            "Rebalance drift threshold (% of target notional):",
            Decimal::from(10),
        )?;

        let liquidation_buffer_pct =
            prompt_decimal("Liquidation buffer (% of mark price):", Decimal::from(15))?;

        let mut map = toml::map::Map::new();
        map.insert(
            "min_funding_threshold".to_string(),
            toml::Value::String(min_funding_threshold.to_string()),
        );
        map.insert(
            "target_notional".to_string(),
            toml::Value::String(target_notional.to_string()),
        );
        map.insert(
            "rebalance_drift_pct".to_string(),
            toml::Value::String(rebalance_drift_pct.to_string()),
        );
        map.insert(
            "liquidation_buffer_pct".to_string(),
            toml::Value::String(liquidation_buffer_pct.to_string()),
        );
        map.insert(
            "spot_instrument".to_string(),
            toml::Value::String(spot_key.to_string()),
        );
        map.insert(
            "perp_instrument".to_string(),
            toml::Value::String(perp_key.to_string()),
        );

        Ok(toml::Value::Table(map))
    }
}
