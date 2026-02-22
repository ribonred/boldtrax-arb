use boldtrax_core::config::types::AppConfig;

pub fn run_doctor() -> anyhow::Result<()> {
    println!("--- Boldtrax Config Doctor ---");

    match AppConfig::load() {
        Ok(config) => {
            println!("[OK] Configuration loaded successfully.");
            println!("Execution Mode: {:?}", config.execution_mode);
            println!("Redis URL: {}", config.redis_url);
            println!("Update Interval: {} ms", config.update_interval_ms);
            println!("Risk Parameters:");
            println!("  Max Leverage: {}", config.risk.max_leverage);
            println!("  Max Position Size: {}", config.risk.max_position_size);
            println!("  Max Total Exposure: {}", config.risk.max_total_exposure);
            println!(
                "  Daily Drawdown Limit: {}%",
                config.risk.daily_drawdown_limit_pct
            );

            println!("Exchanges Configured: {}", config.exchanges.len());
            for (name, exchange_config) in &config.exchanges {
                let testnet = exchange_config
                    .get("testnet")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                println!("  - {} (Testnet: {})", name, testnet);
                if exchange_config.get("api_key").is_some() {
                    println!("    API Key: [PRESENT]");
                } else {
                    println!("    API Key: [MISSING]");
                }
                if exchange_config.get("api_secret").is_some() {
                    println!("    API Secret: [PRESENT]");
                } else {
                    println!("    API Secret: [MISSING]");
                }
            }

            println!("\n[OK] All checks passed.");
        }
        Err(e) => {
            println!("[FAIL] Configuration validation failed.");
            println!("Error: {}", e);
            println!("\nRun `boldtrax-bot wizard` to fix your configuration.");
        }
    }

    Ok(())
}
