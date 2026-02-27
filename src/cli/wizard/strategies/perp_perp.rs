use std::fmt;
use std::str::FromStr;

use ::strategies::arbitrage::runner::StrategyKind;
use inquire::{Select, Text};
use rust_decimal::Decimal;

use super::{StrategyWizard, prompt_decimal, prompt_instrument_key};

pub struct PerpPerpWizard;

impl fmt::Display for PerpPerpWizard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PerpPerp")
    }
}

impl StrategyWizard for PerpPerpWizard {
    fn kind(&self) -> StrategyKind {
        StrategyKind::PerpPerp
    }

    fn prompt(&self) -> anyhow::Result<toml::Value> {
        println!("\n--- PerpPerp Strategy Configuration ---\n");

        let direction_options = vec!["positive", "negative"];
        let carry_direction = Select::new(
            "Carry direction (long leg relative to funding):",
            direction_options,
        )
        .prompt()?;

        println!("Long-leg perp instrument (must be SWAP type):");
        let long_key = loop {
            let key = prompt_instrument_key("Perp long instrument key:", "XRPUSDT-BN-SWAP")?;
            if key.instrument_type != boldtrax_core::types::InstrumentType::Swap {
                println!(
                    "  [WARN] Expected SWAP type, got {:?}. Try again.",
                    key.instrument_type
                );
                continue;
            }
            break key;
        };

        println!("Short-leg perp instrument (must be SWAP type):");
        let short_key = loop {
            let key = prompt_instrument_key("Perp short instrument key:", "XRPUSDT-BY-SWAP")?;
            if key.instrument_type != boldtrax_core::types::InstrumentType::Swap {
                println!(
                    "  [WARN] Expected SWAP type, got {:?}. Try again.",
                    key.instrument_type
                );
                continue;
            }
            break key;
        };

        let min_spread_threshold = prompt_decimal(
            "Minimum |long - short| spread to enter (e.g. 0.0001):",
            Decimal::from_str("0.0001").unwrap(),
        )?;

        let exit_input = Text::new("Exit threshold (leave blank to exit on sign flip):")
            .with_placeholder("e.g. 0.00003")
            .prompt()?;
        let exit_threshold = if exit_input.trim().is_empty() {
            None
        } else {
            Some(
                Decimal::from_str(exit_input.trim())
                    .map_err(|e| anyhow::anyhow!("Invalid decimal: {}", e))?,
            )
        };

        let target_notional =
            prompt_decimal("Target dollar exposure per side (USD):", Decimal::from(280))?;

        let rebalance_drift_pct = prompt_decimal(
            "Rebalance drift threshold (% of target notional):",
            Decimal::from(10),
        )?;

        let liquidation_buffer_pct =
            prompt_decimal("Liquidation buffer (% of mark price):", Decimal::from(15))?;

        let poll_interval_secs = prompt_decimal(
            "Poll interval for funding rates (seconds):",
            Decimal::from(30),
        )?;

        let cache_staleness_secs =
            prompt_decimal("Cache staleness threshold (seconds):", Decimal::from(60))?;

        let mut map = toml::map::Map::new();
        map.insert(
            "carry_direction".to_string(),
            toml::Value::String(carry_direction.to_string()),
        );
        map.insert(
            "min_spread_threshold".to_string(),
            toml::Value::String(min_spread_threshold.to_string()),
        );
        if let Some(exit) = exit_threshold {
            map.insert(
                "exit_threshold".to_string(),
                toml::Value::String(exit.to_string()),
            );
        }
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
            "perp_long_instrument".to_string(),
            toml::Value::String(long_key.to_string()),
        );
        map.insert(
            "perp_short_instrument".to_string(),
            toml::Value::String(short_key.to_string()),
        );
        map.insert(
            "poll_interval_secs".to_string(),
            toml::Value::Integer(poll_interval_secs.try_into().unwrap_or(30)),
        );
        map.insert(
            "cache_staleness_secs".to_string(),
            toml::Value::Integer(cache_staleness_secs.try_into().unwrap_or(60)),
        );

        Ok(toml::Value::Table(map))
    }
}
