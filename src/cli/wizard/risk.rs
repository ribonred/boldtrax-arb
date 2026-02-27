use boldtrax_core::config::types::{FatFingerConfig, RiskConfig};
use inquire::CustomType;
use rust_decimal::Decimal;

pub fn prompt_risk_config() -> anyhow::Result<RiskConfig> {
    println!("--- Risk Configuration ---");

    let max_leverage = CustomType::<Decimal>::new("Enter maximum leverage (e.g., 3.0):")
        .with_default(Decimal::ONE)
        .prompt()?;

    println!("--- Fat-Finger Guards ---");

    let max_order_notional_usd =
        CustomType::<Decimal>::new("Enter max single order notional (USD):")
            .with_default(Decimal::new(100_000, 0))
            .prompt()?;

    let max_price_deviation_pct =
        CustomType::<Decimal>::new("Enter max price deviation from mid-price (%):")
            .with_default(Decimal::new(5, 0))
            .prompt()?;

    Ok(RiskConfig {
        max_leverage,
        fat_finger: FatFingerConfig {
            max_order_notional_usd,
            max_price_deviation_pct,
        },
    })
}
