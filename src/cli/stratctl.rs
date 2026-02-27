//! `stratctl` — CLI for inspecting and controlling running strategies.
//!
//! Discovers strategies via Redis and sends commands to their ZMQ ROUTER
//! sockets using a DEALER client.

use std::time::Duration;

use anyhow::{Context, Result};
use boldtrax_core::config::types::AppConfig;
use boldtrax_core::zmq::discovery::DiscoveryClient;
use boldtrax_core::zmq::protocol::{StrategyCommand, StrategyResponse, StrategyStatus};
use boldtrax_core::zmq::strategy_client::StrategyCommandClient;

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

/// Run the stratctl subcommand.
pub async fn run_stratctl(app_config: &AppConfig, action: StrategyAction) -> Result<()> {
    let redis_url = &app_config.redis_url;

    match action {
        StrategyAction::List => list_strategies(redis_url).await,
        StrategyAction::Status { strategy_id } => {
            let id = resolve_strategy_id(redis_url, strategy_id.as_deref()).await?;
            send_and_print(redis_url, &id, StrategyCommand::GetStatus).await
        }
        StrategyAction::Pause { strategy_id } => {
            let id = resolve_strategy_id(redis_url, strategy_id.as_deref()).await?;
            send_and_print(redis_url, &id, StrategyCommand::Pause).await
        }
        StrategyAction::Resume { strategy_id } => {
            let id = resolve_strategy_id(redis_url, strategy_id.as_deref()).await?;
            send_and_print(redis_url, &id, StrategyCommand::Resume).await
        }
        StrategyAction::Exit { strategy_id, force } => {
            let id = resolve_strategy_id(redis_url, strategy_id.as_deref()).await?;
            let cmd = if force {
                StrategyCommand::ForceExit
            } else {
                StrategyCommand::GracefulExit
            };
            send_and_print(redis_url, &id, cmd).await
        }
    }
}

/// Parsed stratctl action.
#[derive(Debug, Clone)]
pub enum StrategyAction {
    /// List all running strategies.
    List,
    /// Get status of a strategy.
    Status { strategy_id: Option<String> },
    /// Pause evaluation.
    Pause { strategy_id: Option<String> },
    /// Resume evaluation.
    Resume { strategy_id: Option<String> },
    /// Graceful or forced exit.
    Exit {
        strategy_id: Option<String>,
        force: bool,
    },
}

async fn list_strategies(redis_url: &str) -> Result<()> {
    let discovery =
        DiscoveryClient::new(redis_url).context("Failed to connect to Redis for discovery")?;

    let strategies = discovery
        .list_strategies()
        .await
        .context("Failed to list strategies from Redis")?;

    if strategies.is_empty() {
        println!("No strategies currently registered.");
        return Ok(());
    }

    println!("Running strategies ({}):", strategies.len());
    for id in &strategies {
        // Try to get status for each
        match get_status(redis_url, id).await {
            Ok(status) => {
                let paused_marker = if status.paused { " [PAUSED]" } else { "" };
                println!(
                    "  {} — {} | pending_orders: {}{}",
                    status.strategy_id, status.pair_status, status.pending_orders, paused_marker
                );
            }
            Err(_) => {
                println!("  {} — (unreachable)", id);
            }
        }
    }
    Ok(())
}

async fn get_status(redis_url: &str, strategy_id: &str) -> Result<StrategyStatus> {
    let mut client = StrategyCommandClient::discover_and_connect(redis_url, strategy_id).await?;
    let response = client
        .send_command(StrategyCommand::GetStatus, RESPONSE_TIMEOUT)
        .await?;

    match response {
        StrategyResponse::Status(s) => Ok(s),
        other => anyhow::bail!("Unexpected response: {:?}", other),
    }
}

/// If no strategy_id is provided, auto-select the only running strategy.
/// If multiple are running, list them and ask the user to specify.
async fn resolve_strategy_id(redis_url: &str, provided: Option<&str>) -> Result<String> {
    if let Some(id) = provided {
        return Ok(id.to_string());
    }

    let discovery =
        DiscoveryClient::new(redis_url).context("Failed to connect to Redis for discovery")?;
    let strategies = discovery
        .list_strategies()
        .await
        .context("Failed to list strategies")?;

    match strategies.len() {
        0 => anyhow::bail!("No strategies currently registered. Is the bot running?"),
        1 => {
            let id = strategies.into_iter().next().unwrap();
            println!("Auto-selected strategy: {}", id);
            Ok(id)
        }
        _ => {
            eprintln!("Multiple strategies running. Please specify one with --strategy:");
            for id in &strategies {
                eprintln!("  {}", id);
            }
            anyhow::bail!("Ambiguous strategy — use --strategy <id>");
        }
    }
}

async fn send_and_print(redis_url: &str, strategy_id: &str, cmd: StrategyCommand) -> Result<()> {
    println!("Sending {:?} to '{}'...", cmd, strategy_id);

    let mut client = StrategyCommandClient::discover_and_connect(redis_url, strategy_id).await?;
    let response = client
        .send_command(cmd, RESPONSE_TIMEOUT)
        .await
        .context("Failed to get response from strategy")?;

    match response {
        StrategyResponse::Ack => {
            println!("OK — command accepted.");
        }
        StrategyResponse::Rejected(reason) => {
            println!("REJECTED — {}", reason);
        }
        StrategyResponse::Status(s) => {
            print_status(&s);
        }
        StrategyResponse::Error(e) => {
            println!("ERROR — {}", e);
        }
    }

    Ok(())
}

fn print_status(s: &StrategyStatus) {
    println!("Strategy: {}", s.strategy_id);
    println!("  Status:         {}", s.pair_status);
    println!("  Paused:         {}", s.paused);
    println!("  Pending orders: {}", s.pending_orders);
}
