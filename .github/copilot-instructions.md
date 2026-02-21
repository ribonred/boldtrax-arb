# GitHub Copilot Instructions

## Project Context
I'm building a cryptocurrency funding rate arbitrage system in Rust. The system monitors funding rates across multiple exchanges (CEX and DEX), identifies profitable opportunities, executes trades, and manages positions with automated risk controls.

## Core Architecture Principles

### Code Organization
- Use a modular library structure with `core/` for shared abstractions and `exchanges/` for exchange-specific implementations
- Keep exchange logic isolated in separate modules under `exchanges/[exchange_name]/`
- Common types and traits belong in the core library
- Business logic (scanner, position manager, risk monitor) stays separate from exchange connectors

### Exchange Connector Pattern
Every exchange implementation must:
- Implement the standard traits: `MarketData`, `Account`, `Trading`, and the combined `Exchange` trait
- Use the common type system defined in `core/types.rs` for all return values
- Never expose exchange-specific types outside its module boundary
- Handle exchange-specific parsing and conversion internally
- Integrate with the shared metrics collection system for request/response monitoring

### Trait-Based Design
- Define behavior through traits in `core/traits.rs`
- All exchange connectors implement the same trait interfaces
- Use `async_trait` for async trait methods
- Return appropriate error types based on error handling strategy (see Error Handling Philosophy)
- Enable polymorphic usage through `Arc<dyn Exchange>`

### Type System Standards
- Use `rust_decimal::Decimal` for all monetary values and rates, never `f64`
- Use `chrono::DateTime<Utc>` for timestamps
- Define enums for fixed sets of values (OrderSide, OrderType, OrderStatus)
- Keep type definitions in `core/types.rs` for cross-exchange consistency
- Exchange-specific response types stay in `exchanges/[name]/types.rs`

### WebSocket Architecture
- Implement `WebSocketHandler` trait for exchange WebSocket connections
- Use `tokio::sync::mpsc` channels for message passing
- Standardize messages through the `WsMessage` enum
- Handle reconnection logic within the exchange implementation
- Parse exchange-specific formats internally, emit common types

### Metrics and Monitoring
- Wrap all HTTP requests with `MetricsCollector::wrap_request()`
- Record exchange name, endpoint, duration, and success status
- Use channels to send metrics asynchronously
- Track both REST API calls and WebSocket events
- Never block on metrics collection

### Data Persistence Strategy
- Use Redis as the primary data store, no PostgreSQL
- Structure Redis keys hierarchically: `{category}:{exchange}:{symbol}`
- Use sorted sets for time-series data (funding rate history)
- Use hashes for structured data (positions, balances)
- Keep application state in memory, sync critical state to Redis
- Treat exchanges as the source of truth, not local storage

### Mock Trading and Testing Architecture
- Implement a `MockExchange` that mimics real exchange behavior without executing actual trades
- Use a trait-based execution mode system: `ExecutionMode` enum with `Live` and `Paper` variants
- All trading operations must respect the execution mode
- MockExchange maintains simulated positions, balances, and order state in memory
- MockExchange simulates realistic delays, partial fills, and market impact
- Configuration must allow switching between live and paper trading without code changes
- Paper trading results should be stored separately in Redis with a prefix like `paper:{category}:...`

### Configuration System
- Use a centralized configuration structure loaded from TOML/YAML files or environment variables
- Make all risk parameters configurable: position limits, leverage limits, delta thresholds, volatility thresholds
- Support per-exchange configuration overrides
- Include execution mode in configuration (live vs paper trading)
- Allow runtime configuration updates for non-critical parameters
- Validate configuration on startup and reject invalid values
- Use the `config` crate or similar for configuration management

### Risk Management Framework
The risk management system must be:
- **Configurable**: All thresholds and limits defined in configuration files
- **Testable**: Able to run in paper trading mode to validate risk rules
- **Modular**: Risk checks are separate components that can be enabled/disabled
- **Observable**: All risk decisions are logged with reasoning

Risk parameters to make configurable:
- Maximum position size per symbol
- Maximum total exposure across all positions
- Maximum leverage per position
- Delta threshold for rebalancing triggers
- Funding rate thresholds (minimum to enter, maximum negative to exit)
- Open interest change threshold for volatility detection
- Volume spike threshold for volatility detection
- Maximum drawdown limits (daily, weekly)
- Liquidation distance safety margin

### Error Handling Philosophy

**Use explicit error enums (`thiserror`) for:**
- **Core trading operations** (placing orders, canceling orders, closing positions)
- **Risk management decisions** (position validation, leverage checks, risk limit violations)
- **Critical state transitions** (opening positions, rebalancing, emergency shutdowns)
- **Configuration validation** (invalid parameters, missing required fields)
- **Exchange API critical errors** (authentication failures, insufficient balance, position conflicts)

**Use `anyhow::Result` for:**
- **Network/transport errors** (HTTP failures, WebSocket disconnections, timeouts)
- **Retryable operations** (fetching market data, querying balances, transient API errors)
- **Non-critical auxiliary operations** (metrics collection, logging, cache updates)
- **Utility functions** that don't need specific error handling

**Error enum design pattern:**
```rust
use thiserror::Error;

#[derive(Error, Debug)]
pub enum TradingError {
    #[error("Insufficient balance: required {required}, available {available}")]
    InsufficientBalance { required: Decimal, available: Decimal },
    
    #[error("Position size {requested} exceeds limit {limit} for {symbol}")]
    PositionLimitExceeded { symbol: String, requested: Decimal, limit: Decimal },
    
    #[error("Order {order_id} not found on exchange {exchange}")]
    OrderNotFound { exchange: String, order_id: String },
    
    #[error("Invalid execution mode: cannot execute live trade in paper mode")]
    InvalidExecutionMode,
}

#[derive(Error, Debug)]
pub enum RiskError {
    #[error("Leverage {leverage} exceeds maximum {max_leverage} for {symbol}")]
    LeverageExceeded { symbol: String, leverage: Decimal, max_leverage: Decimal },
    
    #[error("Delta {delta} exceeds rebalance threshold {threshold}")]
    DeltaThresholdExceeded { delta: Decimal, threshold: Decimal },
    
    #[error("Daily drawdown {drawdown_pct}% exceeds limit {limit_pct}%")]
    DrawdownLimitExceeded { drawdown_pct: Decimal, limit_pct: Decimal },
    
    #[error("Funding rate {rate} below minimum threshold {min_threshold}")]
    FundingRateBelowThreshold { rate: Decimal, min_threshold: Decimal },
}

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("Missing required configuration: {field}")]
    MissingField { field: String },
    
    #[error("Invalid value for {field}: {value} (expected {expected})")]
    InvalidValue { field: String, value: String, expected: String },
    
    #[error("Configuration validation failed: {reason}")]
    ValidationFailed { reason: String },
}
```

**Error handling guidelines:**
- **Critical errors**: Use explicit enums, never swallow, propagate to top-level for handling
- **Retryable errors**: Use `anyhow`, implement retry logic, log attempts
- **Don't panic**: Only panic on programming errors (e.g., mutex poisoning), never on runtime conditions
- **Provide context**: Always use `.context()` when converting from library errors to `anyhow`
- **Match exhaustively**: When handling custom error enums, handle each variant explicitly
- **Log appropriately**: Critical errors at ERROR level, retryable at WARN, recoverable at INFO

**Conversion pattern:**
```rust
// Converting library errors to anyhow with context
async fn fetch_funding_rate(&self, symbol: &str) -> anyhow::Result<FundingRate> {
    let response = self.client
        .get(&url)
        .send()
        .await
        .context("Failed to connect to exchange API")?;
    
    let data = response
        .json::<ExchangeResponse>()
        .await
        .context("Failed to parse funding rate response")?;
    
    Ok(self.convert_funding_rate(data))
}

// Explicit error for critical operations
async fn place_order(&self, order: OrderRequest) -> Result<Order, TradingError> {
    let balance = self.get_balance(&order.asset).await
        .map_err(|_| TradingError::InsufficientBalance { 
            required: order.size, 
            available: Decimal::ZERO 
        })?;
    
    if balance < order.size {
        return Err(TradingError::InsufficientBalance {
            required: order.size,
            available: balance,
        });
    }
    
    // Execute order...
}
```

### Async and Concurrency
- Use `tokio` as the async runtime
- Spawn separate tasks for each exchange connector
- Use channels for inter-component communication
- Avoid shared mutable state; prefer message passing
- Use `Arc` for sharing immutable data across tasks

### Adding New Exchanges
When I ask you to add a new exchange:
1. Create a new module under `exchanges/[exchange_name]/`
2. Implement all required traits (MarketData, Account, Trading)
3. Create exchange-specific types in `types.rs` within that module
4. Convert exchange responses to common core types before returning
5. Integrate metrics collection in every API call
6. Implement WebSocket handler if needed
7. Define exchange-specific error types if the exchange has unique error conditions
8. Add proper error handling with appropriate error types (explicit enums for critical paths, anyhow for retryable)
9. Create a mock version for testing if the exchange has unique features

### Code Style Preferences
- Prefer explicit over implicit
- Use descriptive variable names
- Keep functions small and focused
- Document complex logic with inline comments
- Use `todo!()` for unimplemented parts during scaffolding
- Avoid premature optimization
- Follow `cargo fmt` and `clippy` guidelines
- Log important events and decisions with context for future debugging

### Dependencies to Use
- `tokio` for async runtime
- `reqwest` for HTTP client
- `tokio-tungstenite` for WebSocket
- `serde` and `serde_json` for serialization
- `sonic_rs` for fast serialization
- `rust_decimal` for precise decimal math
- `chrono` for date/time handling
- `anyhow` for retryable/common errors
- `thiserror` for explicit error enums
- `async-trait` for async traits
- `redis` for Redis client
- `config` or `figment` for configuration management

### What NOT to Do
- Don't use `f64` or `f32` for financial calculations
- Don't expose exchange-specific types in trait method signatures
- Don't mix exchange logic with business logic
- Don't hardcode configuration values
- Don't use blocking operations in async contexts
- Don't duplicate type definitions across exchanges
- Don't use `.unwrap()` or `.expect()` in production code paths
- Don't allow paper trading code to accidentally execute real trades
- Don't bypass risk checks in any execution mode
- Don't use `anyhow` for critical errors that need specific handling
- Don't use custom error enums for simple retryable network errors
- Don't use fullpath imports
- Don't import inside functions, structs, or enums
- Don't use `std::collections::HashSet` instead of `use std::collections::HashSet;`

### Testing Approach
- Write unit tests for type conversions
- Mock external API calls in tests
- Test error handling paths for both explicit errors and anyhow errors
- Keep test data in the test module
- Use `#[tokio::test]` for async tests
- Test risk management rules in paper trading mode before live deployment
- Create integration tests that run complete workflows in mock mode
- Verify that critical errors are properly propagated and handled

## Current Focus
I'm in the early build phase focusing on creating a robust, scalable foundation. The immediate priority is implementing the core architecture correctly so that adding new exchanges is straightforward and consistent. I need the ability to test trading strategies and risk management rules in paper trading mode before risking real capital. All risk parameters must be configurable so I can tune them based on backtesting and paper trading results.

Error handling must be explicit and type-safe for critical operations (trading, risk management, configuration) while remaining pragmatic with anyhow for retryable/transient errors.

When suggesting code, prioritize maintainability and extensibility over cleverness. The goal is a system that's easy to understand and modify as requirements evolve.

## current architectural diagram
[diagram.md](./diagram.md)