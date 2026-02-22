use boldtrax_core::config::types::RiskConfig;
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

    Ok(RiskConfig {
        max_leverage,
        max_position_size,
        max_total_exposure,
        daily_drawdown_limit_pct,
        delta_threshold: Decimal::from_str("0.05").unwrap(),
        min_funding_rate: Decimal::from_str("0.0001").unwrap(),
        max_negative_funding_rate: Decimal::from_str("-0.001").unwrap(),
    })
}
