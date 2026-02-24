pub mod perp_perp;
pub mod spot_perp;

use std::fmt;

use ::strategies::arbitrage::runner::StrategyKind;
use boldtrax_core::types::InstrumentKey;
use inquire::{CustomType, Text};
use rust_decimal::Decimal;

pub trait StrategyWizard: fmt::Display {
    fn kind(&self) -> StrategyKind;
    fn prompt(&self) -> anyhow::Result<toml::Value>;
}

pub fn get_available_strategy_wizards() -> Vec<Box<dyn StrategyWizard>> {
    vec![
        Box::new(spot_perp::SpotPerpWizard),
        Box::new(perp_perp::PerpPerpWizard),
    ]
}

pub fn prompt_instrument_key(label: &str, placeholder: &str) -> anyhow::Result<InstrumentKey> {
    loop {
        let input = Text::new(label).with_placeholder(placeholder).prompt()?;

        let trimmed = input.trim();
        if trimmed.is_empty() {
            println!("  [ERROR] Instrument key cannot be empty.");
            continue;
        }

        match trimmed.parse::<InstrumentKey>() {
            Ok(key) => {
                println!("  ✓ {}", key);
                return Ok(key);
            }
            Err(e) => {
                println!("  [ERROR] {}", e);
                println!("  Format: PAIR-EXCHANGE-TYPE  (e.g. SOLUSDT-BN-SWAP)");
            }
        }
    }
}

pub fn prompt_decimal(message: &str, default: Decimal) -> anyhow::Result<Decimal> {
    let value = CustomType::<Decimal>::new(message)
        .with_default(default)
        .prompt()?;
    Ok(value)
}
