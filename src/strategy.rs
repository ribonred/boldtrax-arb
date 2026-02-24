//! Strategy launcher — builds and spawns strategy runners from configuration.
//!
//! Connects to the exchange's ZMQ server (discovered via Redis) and constructs
//! the appropriate `StrategyRunner` variant based on `[runner.strategies]`.

use std::str::FromStr;
use std::time::Duration;

use anyhow::Context;
use boldtrax_core::CoreApi;
use boldtrax_core::config::types::AppConfig;
use boldtrax_core::types::{Exchange, ExecutionMode};
use boldtrax_core::zmq::client::ZmqClient;
use boldtrax_core::zmq::discovery::{DiscoveryClient, ServiceType};
use boldtrax_core::zmq::router::ZmqRouter;
use strategies::arbitrage::engine::ArbitrageEngine;
use strategies::arbitrage::oracle::PriceOracle;
use strategies::arbitrage::paper::PaperExecution;
use strategies::arbitrage::perp_perp::config::PerpPerpStrategyConfig;
use strategies::arbitrage::perp_perp::execution::PerpPerpExecutionEngine;
use strategies::arbitrage::perp_perp::poller::PerpPerpPoller;
use strategies::arbitrage::perp_perp::types::PerpPerpPair;
use strategies::arbitrage::runner::{StrategyKind, StrategyRunner};
use strategies::arbitrage::spot_perp::config::SpotPerpStrategyConfig;
use strategies::arbitrage::spot_perp::execution::SpotPerpExecutionEngine;
use strategies::arbitrage::spot_perp::types::SpotPerpPair;
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
        StrategyKind::PerpPerp => spawn_perp_perp(app_config).await,
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

    // Build router with a single exchange
    let router = ZmqRouter::connect_all(&app_config.redis_url, &[exchange])
        .await
        .with_context(|| format!("Failed to connect ZMQ router for '{}'", exchange))?;

    // Separate SUB connection for event stream
    let zmq_sub = ZmqClient::connect(&app_config.redis_url, exchange)
        .await
        .with_context(|| format!("Failed to connect ZMQ subscriber for '{}'", exchange))?;
    let (_cmd, event_subscriber) = zmq_sub.split();

    // Set leverage on the perp instrument before trading starts.
    let target_leverage = app_config.risk.max_leverage;
    info!(
        strategy = "SpotPerp",
        instrument = %perp_key,
        target_leverage = %target_leverage,
        "Setting leverage on exchange"
    );
    let actual_leverage = router
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
    let funding_snapshot = router
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
            let execution = PaperExecution::new(SpotPerpExecutionEngine::new(router));
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
            let execution = SpotPerpExecutionEngine::new(router);
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

async fn spawn_perp_perp(app_config: &AppConfig) -> anyhow::Result<JoinHandle<()>> {
    let strategy_config = PerpPerpStrategyConfig::from_strategy_map(&app_config.strategy);

    let exchange_long = strategy_config.exchange_long();
    let exchange_short = strategy_config.exchange_short();
    let long_key = strategy_config.long_key();
    let short_key = strategy_config.short_key();

    info!(
        strategy = "PerpPerp",
        exchange_long = %exchange_long,
        exchange_short = %exchange_short,
        long = %long_key,
        short = %short_key,
        carry = ?strategy_config.carry_direction,
        "Connecting to exchange ZMQ servers"
    );

    // Build router — deduplicates automatically if both legs are on the same exchange.
    let router = ZmqRouter::connect_all(&app_config.redis_url, &[exchange_long, exchange_short])
        .await
        .with_context(|| "Failed to connect ZMQ router for PerpPerp exchanges")?;

    // Separate SUB connections for event streams (one per exchange).
    let zmq_sub_long = ZmqClient::connect(&app_config.redis_url, exchange_long)
        .await
        .with_context(|| {
            format!(
                "Failed to connect ZMQ subscriber for long exchange '{}'",
                exchange_long
            )
        })?;
    let (_cmd_long, sub_long) = zmq_sub_long.split();

    let zmq_sub_short = ZmqClient::connect(&app_config.redis_url, exchange_short)
        .await
        .with_context(|| {
            format!(
                "Failed to connect ZMQ subscriber for short exchange '{}'",
                exchange_short
            )
        })?;
    let (_cmd_short, sub_short) = zmq_sub_short.split();

    // Set leverage on both legs
    let target_leverage = app_config.risk.max_leverage;

    let actual_long = router
        .set_leverage(long_key, target_leverage)
        .await
        .with_context(|| format!("Failed to set leverage for long '{}'", long_key))?;
    info!(
        strategy = "PerpPerp",
        instrument = %long_key,
        actual_leverage = %actual_long,
        "Long leg leverage set"
    );

    let actual_short = router
        .set_leverage(short_key, target_leverage)
        .await
        .with_context(|| format!("Failed to set leverage for short '{}'", short_key))?;
    info!(
        strategy = "PerpPerp",
        instrument = %short_key,
        actual_leverage = %actual_short,
        "Short leg leverage set"
    );

    // Seed initial funding rates from both exchanges
    let snap_long = router
        .get_funding_rate(long_key)
        .await
        .with_context(|| format!("Failed to get funding rate for long '{}'", long_key))?;
    info!(
        strategy = "PerpPerp",
        instrument = %long_key,
        funding_rate = %snap_long.funding_rate,
        mark_price = %snap_long.mark_price,
        "Long funding seeded"
    );

    let snap_short = router
        .get_funding_rate(short_key)
        .await
        .with_context(|| format!("Failed to get funding rate for short '{}'", short_key))?;
    info!(
        strategy = "PerpPerp",
        instrument = %short_key,
        funding_rate = %snap_short.funding_rate,
        mark_price = %snap_short.mark_price,
        "Short funding seeded"
    );

    // Build pair state
    let mut pair = PerpPerpPair::new(
        long_key,
        short_key,
        strategy_config.carry_direction,
        strategy_config.cache_staleness(),
    );
    pair.target_notional = strategy_config.target_notional;
    pair.long_leg.funding_rate = snap_long.funding_rate;
    pair.long_leg.current_price = snap_long.mark_price;
    pair.short_leg.funding_rate = snap_short.funding_rate;
    pair.short_leg.current_price = snap_short.mark_price;
    pair.funding_cache.update(long_key, snap_long);
    pair.funding_cache.update(short_key, snap_short);

    // Build policies
    let oracle = PriceOracle::new();
    let decider = strategy_config.build_decider();
    let margin = strategy_config.build_margin(app_config.risk.max_leverage);

    let execution = PerpPerpExecutionEngine::new(router.clone());

    let poller = PerpPerpPoller {
        pair,
        decider,
        execution,
        margin,
        oracle,
        router,
        poll_interval: strategy_config.poll_interval(),
    };

    let runner = StrategyRunner::PerpPerp(Box::new(poller), sub_long, sub_short);
    info!(
        strategy = "PerpPerp",
        mode = ?app_config.execution_mode,
        poll_interval_secs = strategy_config.poll_interval_secs,
        "Strategy runner spawned"
    );
    let handle = tokio::spawn(async move { runner.run().await });

    Ok(handle)
}
