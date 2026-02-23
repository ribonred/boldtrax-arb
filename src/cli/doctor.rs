use boldtrax_core::config::types::AppConfig;

pub fn run_doctor() -> anyhow::Result<()> {
    println!("--- Boldtrax Config Doctor ---");

    match AppConfig::load() {
        Ok(config) => {
            println!("[OK] Configuration loaded successfully.");
            println!("Execution Mode: {:?}", config.execution_mode);
            println!("Redis URL: {}", config.redis_url);

            println!("\nRunner:");
            println!(
                "  Account Reconcile Interval: {}s",
                config.runner.account_reconcile_interval_secs
            );
            println!(
                "  Funding Rate Poll Interval: {}s",
                config.runner.funding_rate_poll_interval_secs
            );
            println!("  ZMQ TTL: {}s", config.runner.zmq_ttl_secs);

            // ── Exchange runner list ───────────────────────────────────
            if config.runner.exchanges.is_empty() {
                println!("  Exchanges to run: [NONE]");
                println!(
                    "  [WARN] No exchanges in runner.exchanges — run mode won't start any runners"
                );
            } else {
                println!("  Exchanges to run: {:?}", config.runner.exchanges);
                for name in &config.runner.exchanges {
                    if !config.exchanges.contains_key(name) {
                        println!(
                            "  [ERROR] '{}' listed in runner.exchanges but no [exchanges.{}] config found",
                            name, name
                        );
                    }
                }
            }

            // ── Strategy config ───────────────────────────────────────
            if let Some(ref strat) = config.runner.strategies {
                println!("\n  Strategy: {}", strat.strategy);
            } else {
                println!("\n  Strategy: [NONE]");
            }

            println!("\nRisk Parameters:");
            println!("  Max Leverage: {}", config.risk.max_leverage);
            println!("  Max Position Size: {}", config.risk.max_position_size);
            println!("  Max Total Exposure: {}", config.risk.max_total_exposure);
            println!("  Delta Threshold: {}", config.risk.delta_threshold);
            println!("  Min Funding Rate: {}", config.risk.min_funding_rate);
            println!(
                "  Max Negative Funding Rate: {}",
                config.risk.max_negative_funding_rate
            );
            println!(
                "  Daily Drawdown Limit: {}%",
                config.risk.daily_drawdown_limit_pct
            );

            println!("\nFat-Finger Guards:");
            println!(
                "  Max Order Size: {}",
                config.risk.fat_finger.max_order_size
            );
            println!(
                "  Max Order Notional: ${}",
                config.risk.fat_finger.max_order_notional_usd
            );
            println!(
                "  Max Price Deviation: {}%",
                config.risk.fat_finger.max_price_deviation_pct
            );

            println!("\nExchanges Configured: {}", config.exchanges.len());
            for (name, exchange_config) in &config.exchanges {
                let testnet = exchange_config
                    .get("testnet")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let instrument_count = exchange_config
                    .get("instruments")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                println!(
                    "  - {} (Testnet: {}, Instruments: {})",
                    name, testnet, instrument_count
                );
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
                if instrument_count == 0 {
                    println!("    [WARN] No instruments configured");
                }
            }

            if !config.strategy.is_empty() {
                println!("\nStrategies Configured: {}", config.strategy.len());
                for name in config.strategy.keys() {
                    println!("  - {}", name);
                }
            } else {
                println!("\n[WARN] No strategies configured");
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
