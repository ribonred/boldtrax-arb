pub mod binance;
pub mod okx;

use boldtrax_core::types::Exchange;

pub trait ExchangeWizard {
    fn exchange(&self) -> Exchange;
    fn prompt(&self) -> anyhow::Result<toml::Value>;
}

pub fn get_available_wizards() -> Vec<Box<dyn ExchangeWizard>> {
    vec![Box::new(binance::BinanceWizard), Box::new(okx::OkxWizard)]
}
