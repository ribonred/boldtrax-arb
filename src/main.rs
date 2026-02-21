mod runner;

use boldtrax_core::manager::price::PriceManagerConfig;
use boldtrax_core::order::OrderManagerConfig;
use boldtrax_core::position::PositionManagerConfig;
use boldtrax_core::types::{Exchange, ExecutionMode, InstrumentKey, InstrumentType, Pairs};
use boldtrax_core::zmq::server::ZmqServerConfig;
use boldtrax_core::{logger::init_logger, manager::account::AccountManagerConfig};
use dotenvy::dotenv;
use exchanges::binance::{BinanceClient, BinanceConfig};
use exchanges::mock::MockExchange;
use runner::{ExchangeRunner, ExchangeRunnerConfig};
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv().ok();
    let _logguard = init_logger();

    let config = BinanceConfig {
        api_key: std::env::var("BINANCE_API_KEY").ok(),
        api_secret: std::env::var("BINANCE_API_SECRET").ok(),
        ..Default::default()
    };

    let client = BinanceClient::new(config)?;
    let execution_mode = ExecutionMode::Paper;

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
            redis_url: std::env::var("REDIS_URL")
                .unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string()),
            ttl_secs: 30,
        },
    };

    match execution_mode {
        ExecutionMode::Paper => {
            let mock_client = MockExchange::new(client, Exchange::Binance);
            ExchangeRunner::new(mock_client, runner_config)
                .run()
                .await
                .map_err(anyhow::Error::from)
        }
        ExecutionMode::Live => ExchangeRunner::new(client, runner_config)
            .run()
            .await
            .map_err(anyhow::Error::from),
    }
}
