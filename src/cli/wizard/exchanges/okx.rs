use super::ExchangeWizard;
use boldtrax_core::types::Exchange;
use inquire::{Confirm, Text};

pub struct OkxWizard;

impl ExchangeWizard for OkxWizard {
    fn exchange(&self) -> Exchange {
        Exchange::Okx
    }

    fn prompt(&self) -> anyhow::Result<toml::Value> {
        println!("--- OKX Configuration ---");

        let testnet = Confirm::new("Use Testnet?").with_default(true).prompt()?;

        let api_key = Text::new("API Key (leave blank to skip):").prompt()?;
        let api_secret = Text::new("API Secret (leave blank to skip):").prompt()?;
        let passphrase = Text::new("Passphrase (leave blank to skip):").prompt()?;

        let mut map = toml::map::Map::new();
        map.insert("testnet".to_string(), toml::Value::Boolean(testnet));
        if !api_key.is_empty() {
            map.insert("api_key".to_string(), toml::Value::String(api_key));
        }
        if !api_secret.is_empty() {
            map.insert("api_secret".to_string(), toml::Value::String(api_secret));
        }
        if !passphrase.is_empty() {
            map.insert("passphrase".to_string(), toml::Value::String(passphrase));
        }

        Ok(toml::Value::Table(map))
    }
}
