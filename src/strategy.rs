//! Strategy launcher — builds and spawns strategy runners from configuration.
//!
//! Connects to the exchange's ZMQ server (discovered via Redis) and constructs
//! the appropriate `StrategyRunner` variant based on `[runner.strategies]`.

use std::str::FromStr;
use std::time::Duration;

use anyhow::Context;
use boldtrax_core::config::types::AppConfig;
use boldtrax_core::types::{Exchange, ExecutionMode};
use boldtrax_core::zmq::client::ZmqClient;
use boldtrax_core::zmq::discovery::{DiscoveryClient, ServiceType};
use strategies::arbitrage::config::SpotPerpStrategyConfig;
use strategies::arbitrage::engine::ArbitrageEngine;
use strategies::arbitrage::execution::ExecutionEngine;
use strategies::arbitrage::oracle::PriceOracle;
use strategies::arbitrage::paper::PaperExecution;
use strategies::arbitrage::runner::{StrategyKind, StrategyRunner};
use strategies::arbitrage::types::SpotPerpPair;
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// Wait until the exchange's ZMQ services are registered in Redis.
///
/// Polls every 500ms until both PUB and ROUTER endpoints are found,
/// or `timeout` elapses (in which case it logs a warning and returns).
pub async fn poll_until_ready(redis_url: &str, exchange: Exchange, timeout: Duration) {
    let Ok(discovery) = DiscoveryClient::new(redis_url) else {
        warn!("Cannot create Redis client for ZMQ discovery — continuing anyway");
        return;
    };

    let deadline = tokio::time::Instant::now() + timeout;
    let poll_interval = Duration::from_millis(500);

    loop {
        let pub_ok = discovery
            .discover_service(exchange, ServiceType::Pub)
            .await
            .ok()
            .flatten()
            .is_some();
        let router_ok = discovery
            .discover_service(exchange, ServiceType::Router)
            .await
            .ok()
            .flatten()
            .is_some();

        if pub_ok && router_ok {
            info!(
                exchange = %exchange,
                "ZMQ services discovered — strategy can connect"
            );
            return;
        }

        if tokio::time::Instant::now() >= deadline {
            warn!(
                exchange = %exchange,
                "Timed out waiting for ZMQ services — attempting strategy launch anyway"
            );
            return;
        }

        tokio::time::sleep(poll_interval).await;
    }
}

/// Spawn a strategy runner as a tokio task, returning its `JoinHandle`.
///
/// Reads `[runner.strategies]` for the strategy variant name and
/// `[strategy.spot_perp]` for all strategy-specific parameters + instruments.
pub async fn spawn_strategy_runner(
    app_config: &AppConfig,
    strategy_name: &str,
) -> anyhow::Result<JoinHandle<()>> {
    let kind = StrategyKind::from_str(strategy_name)
        .with_context(|| format!("Unknown strategy '{}'", strategy_name))?;

    match kind {
        StrategyKind::SpotPerp => spawn_spot_perp(app_config).await,
    }
}

async fn spawn_spot_perp(app_config: &AppConfig) -> anyhow::Result<JoinHandle<()>> {
    // Load strategy-specific params + instruments from [strategy.spot_perp]
    let strategy_config = SpotPerpStrategyConfig::from_strategy_map(&app_config.strategy);

    let exchange = strategy_config.exchange();
    let spot_key = strategy_config.spot_key();
    let perp_key = strategy_config.perp_key();

    info!(
        strategy = "SpotPerp",
        exchange = %exchange,
        spot = %spot_key,
        perp = %perp_key,
        "Connecting to exchange ZMQ server"
    );

    // Connect to the exchange's ZMQ server via Redis discovery
    let zmq_client = ZmqClient::connect(&app_config.redis_url, exchange)
        .await
        .with_context(|| {
            format!(
                "Failed to connect to ZMQ server for exchange '{}'",
                exchange
            )
        })?;

    let (mut command_client, event_subscriber) = zmq_client.split();

    // Set leverage on the perp instrument before trading starts.
    let target_leverage = app_config.risk.max_leverage;
    info!(
        strategy = "SpotPerp",
        instrument = %perp_key,
        target_leverage = %target_leverage,
        "Setting leverage on exchange"
    );
    let actual_leverage = command_client
        .set_leverage(perp_key, target_leverage)
        .await
        .with_context(|| {
            format!(
                "Failed to set leverage for '{}' to {}",
                perp_key, target_leverage
            )
        })?;
    info!(
        strategy = "SpotPerp",
        instrument = %perp_key,
        actual_leverage = %actual_leverage,
        "Leverage set successfully"
    );
    info!(
        strategy = "SpotPerp",
        instrument = %perp_key,
        "Fetching initial funding rate"
    );
    let funding_snapshot = command_client
        .get_funding_rate(perp_key)
        .await
        .with_context(|| format!("Failed to get funding rate for '{}'", perp_key))?;
    info!(
        strategy = "SpotPerp",
        instrument = %perp_key,
        funding_rate = %funding_snapshot.funding_rate,
        mark_price = %funding_snapshot.mark_price,
        "Seeded funding rate from exchange"
    );

    // Build pair state
    let mut pair = SpotPerpPair::new(spot_key, perp_key);
    pair.target_notional = strategy_config.target_notional;
    pair.perp.funding_rate = funding_snapshot.funding_rate;

    // Build policies
    let oracle = PriceOracle::new();
    let decider = strategy_config.build_decider();
    let margin = strategy_config.build_margin(app_config.risk.max_leverage);

    let execution_mode = app_config.execution_mode;

    let handle = match execution_mode {
        ExecutionMode::Paper => {
            let execution = PaperExecution::new(ExecutionEngine::new(command_client));
            let engine = ArbitrageEngine::new(pair, oracle, decider, margin, execution);
            let runner = StrategyRunner::SpotPerpPaper(engine, event_subscriber);
            info!(
                strategy = "SpotPerp",
                mode = "paper",
                "Strategy runner spawned"
            );
            tokio::spawn(async move { runner.run().await })
        }
        ExecutionMode::Live => {
            let execution = ExecutionEngine::new(command_client);
            let engine = ArbitrageEngine::new(pair, oracle, decider, margin, execution);
            let runner = StrategyRunner::SpotPerp(engine, event_subscriber);
            info!(
                strategy = "SpotPerp",
                mode = "live",
                "Strategy runner spawned"
            );
            tokio::spawn(async move { runner.run().await })
        }
    };

    Ok(handle)
}
