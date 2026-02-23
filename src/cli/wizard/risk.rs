use boldtrax_core::config::types::{FatFingerConfig, RiskConfig};
use inquire::CustomType;
use rust_decimal::Decimal;
use std::str::FromStr;

pub fn prompt_risk_config() -> anyhow::Result<RiskConfig> {
    println!("--- Risk Configuration ---");

    let max_leverage = CustomType::<Decimal>::new("Enter maximum leverage (e.g., 3.0):")
        .with_default(Decimal::ONE)
        .prompt()?;

    let max_position_size =
        CustomType::<Decimal>::new("Enter maximum position size per symbol (USD):")
            .with_default(Decimal::from(1000))
            .prompt()?;

    let max_total_exposure = CustomType::<Decimal>::new("Enter maximum total exposure (USD):")
        .with_default(Decimal::from(5000))
        .prompt()?;

    let daily_drawdown_limit_pct = CustomType::<Decimal>::new("Enter daily drawdown limit (%):")
        .with_default(Decimal::from(5))
        .prompt()?;

    let delta_threshold = CustomType::<Decimal>::new("Enter delta threshold for rebalancing:")
        .with_default(Decimal::from_str("0.05").unwrap())
        .prompt()?;

    let min_funding_rate =
        CustomType::<Decimal>::new("Enter minimum funding rate to enter (e.g., 0.0001):")
            .with_default(Decimal::from_str("0.0001").unwrap())
            .prompt()?;

    let max_negative_funding_rate =
        CustomType::<Decimal>::new("Enter max negative funding rate (e.g., -0.001):")
            .with_default(Decimal::from_str("-0.001").unwrap())
            .prompt()?;

    println!("--- Fat-Finger Guards ---");

    let max_order_size = CustomType::<Decimal>::new("Enter max single order size (base units):")
        .with_default(Decimal::new(10, 0))
        .prompt()?;

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
        max_position_size,
        max_total_exposure,
        daily_drawdown_limit_pct,
        delta_threshold,
        min_funding_rate,
        max_negative_funding_rate,
        fat_finger: FatFingerConfig {
            max_order_size,
            max_order_notional_usd,
            max_price_deviation_pct,
        },
    })
}
