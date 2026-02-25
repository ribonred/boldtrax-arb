use super::{ExchangeWizard, prompt_instruments};
use boldtrax_core::types::Exchange;
use inquire::{Confirm, Text};

pub struct AsterWizard;

impl ExchangeWizard for AsterWizard {
    fn exchange(&self) -> Exchange {
        Exchange::Aster
    }

    fn prompt(&self) -> anyhow::Result<toml::Value> {
        println!("--- Aster Configuration ---");

        let testnet = Confirm::new("Use Testnet?").with_default(true).prompt()?;

        let api_key = Text::new("API Key (leave blank to skip):").prompt()?;
        let api_secret = Text::new("API Secret (leave blank to skip):").prompt()?;

        let mut map = toml::map::Map::new();
        map.insert("testnet".to_string(), toml::Value::Boolean(testnet));
        if !api_key.is_empty() {
            map.insert("api_key".to_string(), toml::Value::String(api_key));
        }
        if !api_secret.is_empty() {
            map.insert("api_secret".to_string(), toml::Value::String(api_secret));
        }

        if Confirm::new("Configure custom base URLs?")
            .with_default(false)
            .prompt()?
        {
            let futures_url = Text::new("Futures Base URL (leave blank for default):").prompt()?;
            if !futures_url.is_empty() {
                map.insert(
                    "futures_base_url".to_string(),
                    toml::Value::String(futures_url),
                );
            }
        }

        let instruments = prompt_instruments(Exchange::Aster)?;
        if !instruments.is_empty() {
            map.insert("instruments".to_string(), toml::Value::Array(instruments));
        }

        Ok(toml::Value::Table(map))
    }
}
