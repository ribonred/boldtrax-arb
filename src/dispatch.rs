//! Exchange dispatch — maps exchange names to their concrete client
//! implementations and spawns an `ExchangeRunner` task for each.
//!
//! Adding a new exchange: match on `exchange_name` and build the client.

use anyhow::{Context, bail};
use boldtrax_core::config::types::{AppConfig, ExchangeConfig};
use boldtrax_core::registry::InstrumentRegistry;
use boldtrax_core::types::{Exchange, ExecutionMode};
use exchanges::aster::client::{AsterClient, AsterConfig};
use exchanges::binance::{BinanceClient, BinanceConfig};
use exchanges::mock::MockExchange;
use tokio::task::JoinHandle;
use tracing::info;

use crate::runner::{ExchangeRunner, ExchangeRunnerConfig, RunnerError};

/// Spawn an `ExchangeRunner` task for the given exchange name.
pub async fn spawn_exchange_runner(
    app_config: &AppConfig,
    exchange_name: &str,
) -> anyhow::Result<JoinHandle<Result<(), RunnerError>>> {
    let exchange = exchange_name
        .parse::<Exchange>()
        .with_context(|| format!("Unknown exchange '{}'", exchange_name))?;

    match exchange_name {
        "binance" => spawn_binance(app_config, exchange).await,
        "aster" => spawn_aster(app_config, exchange).await,
        other => bail!(
            "spawn_exchange_runner: Exchange '{}' is not yet implemented",
            other
        ),
    }
}

async fn spawn_binance(
    app_config: &AppConfig,
    exchange: Exchange,
) -> anyhow::Result<JoinHandle<Result<(), RunnerError>>> {
    let binance_config = BinanceConfig::from_app_config(app_config)?;
    binance_config.validate(app_config.execution_mode);

    let tracked_keys = binance_config.tracked_keys();
    if tracked_keys.is_empty() {
        eprintln!("[FATAL] No instruments configured for Binance.");
        eprintln!("Add instruments to config/exchanges/binance.toml");
        std::process::exit(1);
    }

    info!(
        exchange = "binance",
        instruments = tracked_keys.len(),
        "Starting exchange runner"
    );

    let registry = InstrumentRegistry::new();
    let client = BinanceClient::new(binance_config, registry.clone())?;

    let runner_config = ExchangeRunnerConfig::from_app_config(app_config, exchange, tracked_keys);

    let handle = match app_config.execution_mode {
        ExecutionMode::Paper => {
            let mock = MockExchange::new(client, exchange, registry.clone());
            let runner = ExchangeRunner::new(mock, runner_config, registry);
            tokio::spawn(async move { runner.run().await })
        }
        ExecutionMode::Live => {
            let runner = ExchangeRunner::new(client, runner_config, registry);
            tokio::spawn(async move { runner.run().await })
        }
    };

    Ok(handle)
}

async fn spawn_aster(
    app_config: &AppConfig,
    exchange: Exchange,
) -> anyhow::Result<JoinHandle<Result<(), RunnerError>>> {
    let aster_config = AsterConfig::from_app_config(app_config)?;
    aster_config.validate(app_config.execution_mode);

    let tracked_keys = aster_config.tracked_keys();
    if tracked_keys.is_empty() {
        eprintln!("[FATAL] No instruments configured for Aster.");
        eprintln!("Add instruments to config/exchanges/aster.toml");
        std::process::exit(1);
    }

    info!(
        exchange = "aster",
        instruments = tracked_keys.len(),
        "Starting exchange runner"
    );

    let registry = InstrumentRegistry::new();
    let client = AsterClient::new(aster_config, registry.clone())?;

    let runner_config = ExchangeRunnerConfig::from_app_config(app_config, exchange, tracked_keys);

    let handle = match app_config.execution_mode {
        ExecutionMode::Paper => {
            let mock = MockExchange::new(client, exchange, registry.clone());
            let runner = ExchangeRunner::new(mock, runner_config, registry);
            tokio::spawn(async move { runner.run().await })
        }
        ExecutionMode::Live => {
            let runner = ExchangeRunner::new(client, runner_config, registry);
            tokio::spawn(async move { runner.run().await })
        }
    };

    Ok(handle)
}
