use anyhow::Result;
use boldtrax_core::config::types::{AppConfig, ExchangeConfig};
use boldtrax_core::manager::types::WsPositionPatch;
use boldtrax_core::registry::InstrumentRegistry;
use boldtrax_core::traits::{
    FundingRateMarketData, MarketDataProvider, OrderBookFeeder, OrderExecutionProvider,
    PositionProvider,
};
use boldtrax_core::types::{Exchange, InstrumentKey, OrderEvent};
use chrono::Utc;
use exchanges::aster::client::{AsterClient, AsterConfig};
use exchanges::binance::{BinanceClient, BinanceConfig};
use exchanges::bybit::client::{BybitClient, BybitConfig};
use rust_decimal::Decimal;
use std::str::FromStr;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;

pub async fn run_probe(
    app_config: &AppConfig,
    exchange: Exchange,
    instrument: Option<String>,
) -> Result<()> {
    println!("Probing public endpoints for {:?}", exchange);

    let registry = InstrumentRegistry::new();

    match exchange {
        Exchange::Binance => {
            let config = BinanceConfig::from_app_config(app_config)?;
            let client = BinanceClient::new(config, registry.clone())?;
            probe_client(&client, registry, instrument.clone()).await?;
            probe_account_client(&client).await?;
            probe_position_client(&client, instrument.clone()).await?;
            probe_open_orders(&client, instrument.clone()).await?;
            probe_execution_stream(&client).await?;
            probe_position_stream(&client).await?;
        }
        Exchange::Aster => {
            let config = AsterConfig::from_app_config(app_config)?;
            let client = AsterClient::new(config, registry.clone())?;
            probe_client(&client, registry, instrument.clone()).await?;
            probe_account_client(&client).await?;
            probe_position_client(&client, instrument.clone()).await?;
            probe_open_orders(&client, instrument.clone()).await?;
            probe_execution_stream(&client).await?;
            probe_position_stream(&client).await?;
        }
        Exchange::Bybit => {
            let config = BybitConfig::from_app_config(app_config)?;
            let client = BybitClient::new(config, registry.clone())?;
            probe_client(&client, registry, instrument.clone()).await?;
            probe_account_client(&client).await?;
            probe_position_client(&client, instrument.clone()).await?;
            probe_open_orders(&client, instrument.clone()).await?;
            probe_execution_stream(&client).await?;
            probe_position_stream(&client).await?;
        }
        _ => {
            println!("Probe not implemented for {:?}", exchange);
        }
    }

    Ok(())
}

async fn probe_client<C>(
    client: &C,
    registry: InstrumentRegistry,
    instrument: Option<String>,
) -> Result<()>
where
    C: MarketDataProvider + OrderBookFeeder + FundingRateMarketData,
{
    println!("1. Testing health check...");
    match client.health_check().await {
        Ok(_) => println!("   Health check OK"),
        Err(e) => println!("   Health check failed: {}", e),
    }

    println!("2. Testing server time...");
    match client.server_time().await {
        Ok(time) => println!("   Server time: {}", time),
        Err(e) => println!("   Server time failed: {}", e),
    }

    println!("3. Loading instruments...");
    match client.load_instruments().await {
        Ok(_) => println!("   Loaded {} instruments", registry.len()),
        Err(e) => println!("   Failed to load instruments: {}", e),
    }

    if let Some(inst_str) = instrument {
        let key = InstrumentKey::from_str(&inst_str).map_err(|e| anyhow::anyhow!(e))?;
        println!("4. Probing specific instrument: {}", key);

        if registry.get(&key).is_none() {
            println!("   WARNING: Instrument {} not found in registry", key);
        }

        println!("   Fetching order book...");
        match client.fetch_order_book(key).await {
            Ok(ob) => {
                println!("   Order book fetched successfully:");
                println!("     Best bid: {:?}", ob.best_bid);
                println!("     Best ask: {:?}", ob.best_ask);
                println!("     Spread: {:?}", ob.spread);
            }
            Err(e) => println!("   Failed to fetch order book: {}", e),
        }

        if key.instrument_type == boldtrax_core::types::InstrumentType::Swap {
            println!("   Fetching funding rate snapshot...");
            match client.funding_rate_snapshot(key).await {
                Ok(fr) => {
                    println!("   Funding rate snapshot fetched successfully:");
                    println!("     Funding rate: {}", fr.funding_rate);
                    println!("     Mark price: {}", fr.mark_price);
                    println!("     Next funding: {}", fr.next_funding_time_utc);
                }
                Err(e) => println!("   Failed to fetch funding rate snapshot: {}", e),
            }

            println!("   Fetching funding rate history...");
            let end = Utc::now();
            let start = end - chrono::Duration::days(1);
            match client.funding_rate_history(key, start, end, 10).await {
                Ok(history) => {
                    println!("   Funding rate history fetched successfully:");
                    println!("     Points: {}", history.points.len());
                    if let Some(first) = history.points.first() {
                        println!(
                            "     Latest rate: {} at {}",
                            first.funding_rate, first.event_time_utc
                        );
                    }
                }
                Err(e) => println!("   Failed to fetch funding rate history: {}", e),
            }
        }

        println!("5. Testing WebSocket order book stream (3 seconds)...");
        let (tx, mut rx) = mpsc::channel(100);

        // We can't easily spawn `client.stream_order_books` because `client` is a reference.
        // Instead, we'll run it concurrently with a timeout and a receiver loop.
        let stream_future = client.stream_order_books(vec![key], tx);

        let receive_future = async {
            let mut count = 0;
            while let Some(update) = rx.recv().await {
                count += 1;
                if count == 1 {
                    println!("   Received first WS update!");
                    println!("     Best bid: {:?}", update.snapshot.best_bid);
                    println!("     Best ask: {:?}", update.snapshot.best_ask);
                }
            }
            count
        };

        println!("   Connecting to WebSocket...");
        match timeout(Duration::from_secs(3), async {
            tokio::select! {
                res = stream_future => {
                    if let Err(e) = res {
                        println!("   Stream error: {}", e);
                    }
                    0
                }
                count = receive_future => count,
            }
        })
        .await
        {
            Ok(count) => println!("   Stream ended early. Received {} updates.", count),
            Err(_) => {
                // Timeout reached, which is expected for a continuous stream
                // The `rx` will be dropped here, which should close the channel and signal the stream to stop
                println!("   Successfully streamed for 3 seconds.");
            }
        }
    } else {
        println!("No instrument specified. Skipping instrument-specific probes.");
        println!("Try running with `--instrument <KEY>` (e.g., BTCUSDT-AS-SWAP)");
    }

    Ok(())
}

async fn probe_account_client<C>(client: &C) -> Result<()>
where
    C: boldtrax_core::traits::Account,
{
    println!("6. Testing account snapshot...");
    match client.account_snapshot().await {
        Ok(snapshot) => {
            println!("   Account snapshot fetched successfully:");
            println!("     Model: {:?}", snapshot.model);
            println!("     Partitions: {}", snapshot.partitions.len());
            println!("     Balances:");
            for balance in snapshot.balances {
                if balance.total > rust_decimal::Decimal::ZERO {
                    println!(
                        "       {}: Total: {}, Free: {}, Locked: {}, Unrealized PnL: {}",
                        balance.asset,
                        balance.total,
                        balance.free,
                        balance.locked,
                        balance.unrealized_pnl
                    );
                }
            }
        }
        Err(e) => println!("   Failed to fetch account snapshot: {}", e),
    }

    Ok(())
}

async fn probe_position_client<C>(client: &C, instrument: Option<String>) -> Result<()>
where
    C: boldtrax_core::traits::PositionProvider + boldtrax_core::traits::LeverageProvider,
{
    println!("7. Testing positions...");
    match client.fetch_positions().await {
        Ok(positions) => {
            println!(
                "   Positions fetched successfully: {} total",
                positions.len()
            );
            for pos in positions {
                println!(
                    "     {}: Size: {}, Entry: {}, PnL: {}, Leverage: {}",
                    pos.key, pos.size, pos.entry_price, pos.unrealized_pnl, pos.leverage
                );
            }
        }
        Err(e) => println!("   Failed to fetch positions: {}", e),
    }

    if let Some(inst_str) = instrument {
        let key = InstrumentKey::from_str(&inst_str).map_err(|e| anyhow::anyhow!(e))?;
        if key.instrument_type == boldtrax_core::types::InstrumentType::Swap {
            println!("8. Testing leverage for {}...", key);

            let target_leverage = Decimal::from_str("2").unwrap_or(Decimal::ONE);
            println!("   Setting leverage to {}...", target_leverage);
            match client.set_leverage(key, target_leverage).await {
                Ok(_) => println!("   Successfully set leverage to {}", target_leverage),
                Err(e) => println!("   Failed to set leverage: {}", e),
            }

            match client.get_leverage(key).await {
                Ok(Some(lev)) => println!("   Current leverage (cached): {}", lev),
                Ok(None) => println!("   Current leverage (cached): None"),
                Err(e) => println!("   Failed to get leverage: {}", e),
            }
        }
    }

    Ok(())
}

async fn probe_open_orders<C>(client: &C, instrument: Option<String>) -> Result<()>
where
    C: OrderExecutionProvider,
{
    let Some(inst_str) = instrument else {
        println!("9. Skipping open orders (no --instrument).");
        return Ok(());
    };

    let key = InstrumentKey::from_str(&inst_str).map_err(|e| anyhow::anyhow!(e))?;
    println!("9.  Testing get_open_orders for {}...", key);
    match client.get_open_orders(key).await {
        Ok(orders) => {
            println!("    Open orders: {} total", orders.len());
            for order in &orders {
                println!(
                    "      {} {} {} | size: {} filled: {} | status: {:?} | id: {}",
                    order.request.key,
                    order.request.side,
                    order.request.order_type,
                    order.request.size,
                    order.filled_size,
                    order.status,
                    order.client_order_id,
                );
            }
        }
        Err(e) => println!("    Failed to fetch open orders: {}", e),
    }

    Ok(())
}

async fn probe_execution_stream<C>(client: &C) -> Result<()>
where
    C: OrderExecutionProvider,
{
    println!("10. Testing execution stream (5 seconds)...");
    let (tx, mut rx) = mpsc::channel::<OrderEvent>(64);

    let stream_future = client.stream_executions(tx);

    let receive_future = async {
        let mut count = 0u32;
        while let Some(event) = rx.recv().await {
            count += 1;
            let order = event.inner();
            println!(
                "    [exec #{count}] {} {:?} {} | filled: {} | status: {:?}",
                order.request.key,
                order.request.side,
                order.request.order_type,
                order.filled_size,
                order.status,
            );
        }
        count
    };

    println!("    Connecting...");
    match timeout(Duration::from_secs(5), async {
        tokio::select! {
            res = stream_future => {
                if let Err(e) = res {
                    println!("    Stream error: {}", e);
                }
                0u32
            }
            count = receive_future => count,
        }
    })
    .await
    {
        Ok(count) => println!("    Stream ended early. Received {} events.", count),
        Err(_) => println!("    Timed out after 5s (expected for idle stream)."),
    }

    Ok(())
}

async fn probe_position_stream<C>(client: &C) -> Result<()>
where
    C: PositionProvider,
{
    println!("11. Testing position stream (5 seconds)...");
    let (tx, mut rx) = mpsc::channel::<WsPositionPatch>(64);

    let stream_future = client.stream_position_updates(tx);

    let receive_future = async {
        let mut count = 0u32;
        while let Some(patch) = rx.recv().await {
            count += 1;
            println!(
                "    [pos #{count}] {} | size: {} | entry: {} | pnl: {}",
                patch.key, patch.size, patch.entry_price, patch.unrealized_pnl,
            );
        }
        count
    };

    println!("    Connecting...");
    match timeout(Duration::from_secs(5), async {
        tokio::select! {
            res = stream_future => {
                if let Err(e) = res {
                    println!("    Stream error: {}", e);
                }
                0u32
            }
            count = receive_future => count,
        }
    })
    .await
    {
        Ok(count) => println!("    Stream ended early. Received {} patches.", count),
        Err(_) => println!("    Timed out after 5s (expected for idle stream)."),
    }

    Ok(())
}
