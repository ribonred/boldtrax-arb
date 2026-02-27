mod cli;
mod dispatch;
mod runner;
mod strategy;

use std::time::Duration;

use std::str::FromStr;

use boldtrax_core::config::types::AppConfig;
use boldtrax_core::types::Exchange;
use clap::{Parser, Subcommand, ValueEnum};
use dotenvy::dotenv;
use strategies::arbitrage::perp_perp::config::PerpPerpStrategyConfig;
use strategies::arbitrage::runner::StrategyKind;
use strategies::arbitrage::spot_perp::config::SpotPerpStrategyConfig;
use tracing::{error, info, warn};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run exchange runners, strategy runners, or both
    Run {
        /// What to run: exchange runners, strategy, or all
        #[arg(short, long, default_value = "all")]
        mode: RunMode,
    },
    /// Interactive manual trading REPL
    Manual {
        /// Exchange to connect to via ZMQ
        #[arg(short, long, default_value = "binance")]
        exchange: Exchange,
    },
    /// Probe public endpoints of an exchange
    Probe {
        /// Exchange to probe
        #[arg(short, long)]
        exchange: Exchange,
        /// Instrument key to probe (e.g., BTCUSDT-AS-SWAP)
        #[arg(short, long)]
        instrument: Option<String>,
    },
    /// Initialize configuration via interactive wizard
    Wizard,
    /// Check configuration health
    Doctor,
    /// Control running strategies (list, status, pause, resume, exit)
    #[command(alias = "ctl")]
    Stratctl {
        #[command(subcommand)]
        action: StratctlSubcommand,
    },
}

#[derive(Subcommand)]
enum StratctlSubcommand {
    /// List all running strategies
    List,
    /// Get current status of a strategy
    Status {
        /// Strategy ID (auto-selected if only one is running)
        #[arg(short, long)]
        strategy: Option<String>,
    },
    /// Pause strategy evaluation
    Pause {
        /// Strategy ID (auto-selected if only one is running)
        #[arg(short, long)]
        strategy: Option<String>,
    },
    /// Resume strategy evaluation
    Resume {
        /// Strategy ID (auto-selected if only one is running)
        #[arg(short, long)]
        strategy: Option<String>,
    },
    /// Gracefully unwind positions (chunked close)
    Exit {
        /// Strategy ID (auto-selected if only one is running)
        #[arg(short, long)]
        strategy: Option<String>,
        /// Force immediate market-close instead of gradual unwind
        #[arg(short, long, default_value = "false")]
        force: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum RunMode {
    /// Run exchange runners + strategy (monolith)
    All,
    /// Run exchange runners only (no strategy)
    Exchange,
    /// Run strategy only (connect to external exchange runner via ZMQ)
    Strategy,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv().ok();
    tracing_config::init!();

    let cli = Cli::parse();

    // Default to `Run --mode all` when no subcommand is given
    let command = cli.command.unwrap_or(Commands::Run { mode: RunMode::All });

    match command {
        Commands::Wizard => {
            cli::wizard::run_wizard()?;
        }
        Commands::Doctor => {
            cli::doctor::run_doctor()?;
        }
        Commands::Manual { exchange } => {
            let app_config = load_and_validate_config();
            cli::manual::run_manual(&app_config, exchange).await?;
        }
        Commands::Probe {
            exchange,
            instrument,
        } => {
            let app_config = load_and_validate_config();
            cli::probe::run_probe(&app_config, exchange, instrument).await?;
        }
        Commands::Run { mode } => {
            let app_config = load_and_validate_config();

            println!(
                "Starting Boldtrax in {:?} mode (run mode: {:?})...",
                app_config.execution_mode, mode
            );

            match mode {
                RunMode::Exchange => run_exchanges(&app_config).await?,
                RunMode::Strategy => run_strategy(&app_config).await?,
                RunMode::All => run_all(&app_config).await?,
            }
        }
        Commands::Stratctl { action } => {
            let app_config = load_and_validate_config();
            let strategy_action = match action {
                StratctlSubcommand::List => cli::stratctl::StrategyAction::List,
                StratctlSubcommand::Status { strategy } => cli::stratctl::StrategyAction::Status {
                    strategy_id: strategy,
                },
                StratctlSubcommand::Pause { strategy } => cli::stratctl::StrategyAction::Pause {
                    strategy_id: strategy,
                },
                StratctlSubcommand::Resume { strategy } => cli::stratctl::StrategyAction::Resume {
                    strategy_id: strategy,
                },
                StratctlSubcommand::Exit { strategy, force } => {
                    cli::stratctl::StrategyAction::Exit {
                        strategy_id: strategy,
                        force,
                    }
                }
            };
            cli::stratctl::run_stratctl(&app_config, strategy_action).await?;
        }
    }

    Ok(())
}

/// Load configuration, validate, and exit on failure.
fn load_and_validate_config() -> AppConfig {
    let app_config = AppConfig::load().unwrap_or_else(|e| {
        eprintln!("Failed to load configuration: {}", e);
        eprintln!("Run `boldtrax-bot doctor` to check your configuration.");
        std::process::exit(1);
    });

    let config_errors = app_config.validate();
    if !config_errors.is_empty() {
        for err in &config_errors {
            eprintln!("[CONFIG ERROR] {}", err);
        }
        eprintln!("Run `boldtrax-bot doctor` to diagnose.");
        std::process::exit(1);
    }

    app_config
}

/// Run only exchange runners (no strategy).
async fn run_exchanges(app_config: &AppConfig) -> anyhow::Result<()> {
    let exchanges = &app_config.runner.exchanges;
    if exchanges.is_empty() {
        eprintln!("[FATAL] No exchanges configured in runner.exchanges");
        std::process::exit(1);
    }

    let mut handles = Vec::new();
    for name in exchanges {
        let handle = dispatch::spawn_exchange_runner(app_config, name).await?;
        handles.push(handle);
    }

    info!(
        count = handles.len(),
        "Exchange runners started — press Ctrl-C to stop"
    );

    tokio::select! {
        _ = async {
            for handle in handles {
                if let Err(e) = handle.await {
                    error!(error = %e, "Exchange runner task failed");
                }
            }
        } => {}
        _ = tokio::signal::ctrl_c() => {
            info!("Ctrl-C received — shutting down");
        }
    }

    Ok(())
}

/// Run only strategy (connects to external exchange runner via ZMQ).
async fn run_strategy(app_config: &AppConfig) -> anyhow::Result<()> {
    let runner_strategy = app_config
        .runner
        .strategies
        .as_ref()
        .expect("[runner.strategies] is required for --mode strategy");

    let handle = strategy::spawn_strategy_runner(app_config, &runner_strategy.strategy).await?;

    tokio::select! {
        res = handle => {
            if let Err(e) = res {
                error!(error = %e, "Strategy runner task failed");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Ctrl-C received — shutting down");
        }
    }

    Ok(())
}

/// Run exchange runners + strategy in the same process.
async fn run_all(app_config: &AppConfig) -> anyhow::Result<()> {
    // 1. Start exchange runners
    let exchanges = &app_config.runner.exchanges;
    if exchanges.is_empty() {
        eprintln!("[FATAL] No exchanges configured in runner.exchanges");
        std::process::exit(1);
    }

    let mut exchange_handles = Vec::new();
    for name in exchanges {
        let handle = dispatch::spawn_exchange_runner(app_config, name).await?;
        exchange_handles.push(handle);
    }

    info!(count = exchange_handles.len(), "Exchange runners started");

    // 2. Wait for ZMQ services to register before launching strategy
    let mut strategy_handle = None;
    if let Some(runner_strategy) = &app_config.runner.strategies {
        // Derive exchange(s) from the strategy config to wait for ZMQ discovery.
        let kind = StrategyKind::from_str(&runner_strategy.strategy).unwrap_or_else(|_| {
            eprintln!(
                "[FATAL] Unknown strategy '{}' in runner.strategies",
                runner_strategy.strategy
            );
            std::process::exit(1);
        });

        let exchanges_to_wait: Vec<Exchange> = match kind {
            StrategyKind::SpotPerp => {
                let cfg = SpotPerpStrategyConfig::from_strategy_map(&app_config.strategy);
                vec![cfg.exchange()]
            }
            StrategyKind::PerpPerp => {
                let cfg = PerpPerpStrategyConfig::from_strategy_map(&app_config.strategy);
                let mut exs = vec![cfg.exchange_long()];
                if cfg.exchange_short() != cfg.exchange_long() {
                    exs.push(cfg.exchange_short());
                }
                exs
            }
        };

        // Exchange runner needs time to: load instruments, spawn managers,
        // bind ZMQ sockets, and register in Redis. Give it a generous timeout.
        for exchange in &exchanges_to_wait {
            strategy::poll_until_ready(&app_config.redis_url, *exchange, Duration::from_secs(30))
                .await;
        }

        match strategy::spawn_strategy_runner(app_config, &runner_strategy.strategy).await {
            Ok(handle) => {
                strategy_handle = Some(handle);
                info!("Strategy '{}' started", runner_strategy.strategy);
            }
            Err(e) => {
                error!(error = %e, strategy = %runner_strategy.strategy, "Failed to start strategy — exchange runners continue");
            }
        }
    } else {
        info!("No [runner.strategies] configured — running exchange runners only");
    }

    tokio::select! {
        _ = async {
            for handle in exchange_handles {
                if let Err(e) = handle.await {
                    error!(error = %e, "Exchange runner task failed");
                }
            }
        } => {}
        _ = async {
            if let Some(h) = strategy_handle {
                if let Err(e) = h.await {
                    error!(error = %e, "Strategy runner task failed");
                }
            } else {
                // No strategy — just pend forever (Ctrl-C will break)
                warn!("No strategy runner — awaiting Ctrl-C to exit");
                std::future::pending::<()>().await;
            }
        } => {}
        _ = tokio::signal::ctrl_c() => {
            info!("Ctrl-C received — shutting down");
        }
    }

    Ok(())
}
