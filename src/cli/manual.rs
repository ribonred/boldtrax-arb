//! Interactive REPL for manual trading via ZMQ.
//!
//! Moved from the old `strategies/src/main.rs` into the main binary
//! so all entry points live in one place.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, RwLock};

use anyhow::Result;
use boldtrax_core::FundingRateSnapshot;
use boldtrax_core::config::types::AppConfig;
use boldtrax_core::types::{
    Exchange, InstrumentKey, InstrumentType, OrderRequest, OrderSide, OrderType, Pairs,
};
use boldtrax_core::zmq::protocol::ZmqEvent;
use rust_decimal::Decimal;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use strategies::manual::ManualStrategy;
use tracing::info;

pub async fn run_manual(app_config: &AppConfig, exchange: Exchange) -> Result<()> {
    info!("Starting Manual Strategy REPL for {:?}", exchange);

    let mut strategy = ManualStrategy::new(&app_config.redis_url, exchange).await?;
    let mut event_rx = strategy.subscribe();
    let fundingrate_store: Arc<RwLock<HashMap<InstrumentKey, FundingRateSnapshot>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let funding_clone = fundingrate_store.clone();

    // Spawn a background task to print events
    tokio::spawn(async move {
        while let Ok(event) = event_rx.recv().await {
            match event {
                ZmqEvent::OrderUpdate(update) => {
                    println!("\n[EVENT] Order Update: {:?}", update);
                }
                ZmqEvent::Trade(_trade) => {}
                ZmqEvent::PositionUpdate(pos) => {
                    println!("\n[EVENT] Position Update: {:?}", pos);
                }
                ZmqEvent::FundingRate(snap) => {
                    if let Ok(mut funding) = funding_clone.write() {
                        funding.insert(snap.key, snap);
                    }
                }
                _ => {}
            }
        }
    });

    let mut rl = DefaultEditor::new()?;
    println!("Connected to ExchangeRunner. Type 'help' for commands.");
    loop {
        let readline = rl.readline(">> ");
        match readline {
            Ok(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                rl.add_history_entry(line)?;

                let parts: Vec<&str> = line.split_whitespace().collect();
                match parts[0] {
                    "help" => {
                        println!("Available commands:");
                        println!("  balance");
                        println!("  positions");
                        println!("  position <pair> <type>");
                        println!("  instruments");
                        println!("  fundingrate <instrument_key>");
                        println!("  buy <pair> <type> <size> [price]");
                        println!("  sell <pair> <type> <size> [price]");
                        println!("  cancel <order_id>");
                        println!("  exit");
                    }
                    "fundingrate" => {
                        if parts.len() < 2 {
                            println!("Usage: fundingrate <instrument_key>");
                            continue;
                        }
                        let key = match InstrumentKey::from_str(parts[1]) {
                            Ok(k) => k,
                            Err(_) => {
                                println!("Invalid instrument key");
                                continue;
                            }
                        };
                        if let Ok(funding) = fundingrate_store.read() {
                            match funding.get(&key) {
                                Some(rate) => println!("{:#?}", rate),
                                None => println!("No funding rate data for this instrument"),
                            }
                        }
                    }
                    "balance" => match strategy.get_account_snapshot().await {
                        Ok(snapshot) => println!("{:#?}", snapshot),
                        Err(e) => println!("Error: {}", e),
                    },
                    "positions" => match strategy.get_all_positions().await {
                        Ok(positions) => println!("{:#?}", positions),
                        Err(e) => println!("Error: {}", e),
                    },
                    "position" => {
                        if parts.len() < 3 {
                            println!("Usage: position <pair> <type>");
                            continue;
                        }
                        let pair = match Pairs::from_str(parts[1]) {
                            Ok(p) => p,
                            Err(_) => {
                                println!("Invalid pair");
                                continue;
                            }
                        };
                        let instrument_type = match InstrumentType::from_str(parts[2]) {
                            Ok(t) => t,
                            Err(_) => {
                                println!("Invalid instrument type");
                                continue;
                            }
                        };
                        let key = InstrumentKey {
                            exchange,
                            pair,
                            instrument_type,
                        };
                        match strategy.get_position(key).await {
                            Ok(pos) => println!("{:#?}", pos),
                            Err(e) => println!("Error: {}", e),
                        }
                    }
                    "instruments" => match strategy.get_all_instruments().await {
                        Ok(instruments) => println!("{:#?}", instruments),
                        Err(e) => println!("Error: {}", e),
                    },
                    "buy" | "sell" => {
                        if parts.len() < 4 {
                            println!("Usage: {} <pair> <type> <size> [price]", parts[0]);
                            continue;
                        }
                        let side = if parts[0] == "buy" {
                            OrderSide::Buy
                        } else {
                            OrderSide::Sell
                        };
                        let pair = match Pairs::from_str(parts[1]) {
                            Ok(p) => p,
                            Err(_) => {
                                println!("Invalid pair");
                                continue;
                            }
                        };
                        let instrument_type = match InstrumentType::from_str(parts[2]) {
                            Ok(t) => t,
                            Err(_) => {
                                println!("Invalid instrument type");
                                continue;
                            }
                        };
                        let size = match Decimal::from_str(parts[3]) {
                            Ok(s) => s,
                            Err(_) => {
                                println!("Invalid size");
                                continue;
                            }
                        };
                        let price = if parts.len() > 4 {
                            match Decimal::from_str(parts[4]) {
                                Ok(p) => Some(p),
                                Err(_) => {
                                    println!("Invalid price");
                                    continue;
                                }
                            }
                        } else {
                            None
                        };

                        let order_type = if price.is_some() {
                            OrderType::Limit
                        } else {
                            OrderType::Market
                        };

                        let req = OrderRequest {
                            key: InstrumentKey {
                                exchange,
                                pair,
                                instrument_type,
                            },
                            strategy_id: "manual_repl".to_string(),
                            side,
                            order_type,
                            price,
                            size,
                            post_only: false,
                            reduce_only: false,
                        };

                        match strategy.submit_order(req).await {
                            Ok(order) => println!("Order submitted: {:#?}", order),
                            Err(e) => println!("Error: {}", e),
                        }
                    }
                    "cancel" => {
                        if parts.len() < 2 {
                            println!("Usage: cancel <order_id>");
                            continue;
                        }
                        match strategy.cancel_order(parts[1].to_string()).await {
                            Ok(order) => println!("Order canceled: {:#?}", order),
                            Err(e) => println!("Error: {}", e),
                        }
                    }
                    "exit" | "quit" => break,
                    _ => println!("Unknown command. Type 'help' for commands."),
                }
            }
            Err(ReadlineError::Interrupted) => {
                println!("CTRL-C");
                break;
            }
            Err(ReadlineError::Eof) => {
                println!("CTRL-D");
                break;
            }
            Err(err) => {
                println!("Error: {:?}", err);
                break;
            }
        }
    }

    Ok(())
}
