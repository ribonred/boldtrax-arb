use super::{ExchangeWizard, prompt_instruments};
use boldtrax_core::types::Exchange;
use inquire::{Confirm, Text};

pub struct BybitWizard;

impl ExchangeWizard for BybitWizard {
    fn exchange(&self) -> Exchange {
        Exchange::Bybit
    }

    fn prompt(&self) -> anyhow::Result<toml::Value> {
        println!("--- Bybit Configuration ---");

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

        let account_type = Text::new("Account type (UNIFIED/CONTRACT):")
            .with_default("UNIFIED")
            .prompt()?;
        map.insert(
            "account_type".to_string(),
            toml::Value::String(account_type),
        );

        let instruments = prompt_instruments(Exchange::Bybit)?;
        if !instruments.is_empty() {
            map.insert("instruments".to_string(), toml::Value::Array(instruments));
        }

        Ok(toml::Value::Table(map))
    }
}
