pub mod binance;
pub mod okx;

use boldtrax_core::types::{Exchange, InstrumentKey};
use inquire::{Confirm, Text};

pub trait ExchangeWizard {
    fn exchange(&self) -> Exchange;
    fn prompt(&self) -> anyhow::Result<toml::Value>;
}

pub fn get_available_wizards() -> Vec<Box<dyn ExchangeWizard>> {
    vec![Box::new(binance::BinanceWizard), Box::new(okx::OkxWizard)]
}

pub fn prompt_instruments(exchange: Exchange) -> anyhow::Result<Vec<toml::Value>> {
    if !Confirm::new("Configure tracked instruments?")
        .with_default(true)
        .prompt()?
    {
        return Ok(vec![]);
    }

    let short = exchange.short_code();
    println!(
        "Enter instruments in PAIR-{}-TYPE format (e.g. SOLUSDT-{}-SWAP).",
        short, short
    );
    println!("Leave blank and press Enter when done.\n");

    let mut instruments = Vec::new();

    loop {
        let input = Text::new("Instrument key:")
            .with_placeholder(&format!("SOLUSDT-{}-SWAP", short))
            .prompt()?;

        let trimmed = input.trim();
        if trimmed.is_empty() {
            break;
        }

        match trimmed.parse::<InstrumentKey>() {
            Ok(key) => {
                if key.exchange != exchange {
                    println!(
                        "  [WARN] Exchange mismatch: expected {} ({}), got {}. Skipped.",
                        exchange,
                        short,
                        key.exchange.short_code()
                    );
                    continue;
                }

                let key_str = key.to_string();
                println!("  Added: {}", key_str);
                instruments.push(toml::Value::String(key_str));
            }
            Err(e) => {
                println!("  [ERROR] {}", e);
                println!(
                    "  Format: PAIR-{}-TYPE  (e.g. BTCUSDT-{}-SPOT)",
                    short, short
                );
            }
        }
    }

    if instruments.is_empty() {
        println!("No instruments configured.");
    } else {
        println!("Added {} instrument(s).", instruments.len());
    }

    Ok(instruments)
}
