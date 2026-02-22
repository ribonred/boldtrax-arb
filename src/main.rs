mod cli;
mod runner;

use boldtrax_core::config::types::{AppConfig, ExchangeConfig};
use boldtrax_core::manager::price::PriceManagerConfig;
use boldtrax_core::order::OrderManagerConfig;
use boldtrax_core::position::PositionManagerConfig;
use boldtrax_core::registry::InstrumentRegistry;
use boldtrax_core::types::{Exchange, ExecutionMode, InstrumentKey, InstrumentType, Pairs};
use boldtrax_core::zmq::server::ZmqServerConfig;
use boldtrax_core::{logger::init_logger, manager::account::AccountManagerConfig};
use clap::{Parser, Subcommand};
use dotenvy::dotenv;
use exchanges::binance::{BinanceClient, BinanceConfig};
use exchanges::mock::MockExchange;
use runner::{ExchangeRunner, ExchangeRunnerConfig};
use std::time::Duration;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize configuration via interactive wizard
    Wizard,
    /// Check configuration health
    Doctor,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv().ok();
    let _logguard = init_logger();

    let cli = Cli::parse();

    match &cli.command {
        Some(Commands::Wizard) => {
            cli::wizard::run_wizard()?;
            return Ok(());
        }
        Some(Commands::Doctor) => {
            cli::doctor::run_doctor()?;
            return Ok(());
        }
        None => {
            // Normal startup
            let app_config = AppConfig::load().unwrap_or_else(|e| {
                eprintln!("Failed to load configuration: {}", e);
                eprintln!("Run `boldtrax-bot doctor` to check your configuration.");
                std::process::exit(1);
            });

            println!(
                "Starting Boldtrax in {:?} mode...",
                app_config.execution_mode
            );

            let config = BinanceConfig::from_app_config(&app_config)?;
            println!("Loaded Binance config: {:?}", config);

            let registry = InstrumentRegistry::new();
            let client = BinanceClient::new(config, registry.clone())?;
            let execution_mode = app_config.execution_mode;

            // Instruments to track across all sub-systems.
            let tracked_keys = vec![
                InstrumentKey {
                    exchange: Exchange::Binance,
                    pair: Pairs::SOLUSDT,
                    instrument_type: InstrumentType::Swap,
                },
                InstrumentKey {
                    exchange: Exchange::Binance,
                    pair: Pairs::SOLUSDT,
                    instrument_type: InstrumentType::Spot,
                },
                InstrumentKey {
                    exchange: Exchange::Binance,
                    pair: Pairs::USDCUSDT,
                    instrument_type: InstrumentType::Spot,
                },
            ];

            let runner_config = ExchangeRunnerConfig {
                exchange: Exchange::Binance,
                execution_mode,
                account_reconcile_interval: Duration::from_secs(30),
                funding_rate_poll_interval: Duration::from_secs(10),
                tracked_keys,
                account_manager_config: AccountManagerConfig {
                    mailbox_capacity: 64,
                    snapshot_max_age: chrono::Duration::seconds(30),
                },
                price_manager_config: PriceManagerConfig::default(),
                order_manager_config: OrderManagerConfig::default(),
                position_manager_config: PositionManagerConfig::default(),
                zmq_config: ZmqServerConfig {
                    exchange: Exchange::Binance,
                    redis_url: app_config.redis_url.clone(),
                    ttl_secs: 30,
                },
            };

            match execution_mode {
                ExecutionMode::Paper => {
                    let mock_client =
                        MockExchange::new(client, Exchange::Binance, registry.clone());
                    ExchangeRunner::new(mock_client, runner_config, registry.clone())
                        .run()
                        .await
                        .map_err(anyhow::Error::from)
                }
                ExecutionMode::Live => ExchangeRunner::new(client, runner_config, registry.clone())
                    .run()
                    .await
                    .map_err(anyhow::Error::from),
            }
        }
    }
}
