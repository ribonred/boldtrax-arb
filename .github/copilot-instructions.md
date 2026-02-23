# GitHub Copilot Instructions

## Project Context
I'm building a cryptocurrency funding rate arbitrage system in Rust. The system monitors funding rates across multiple exchanges (CEX and DEX), identifies profitable opportunities, executes trades, and manages positions with automated risk controls.

## Core Architecture Principles

**CRITICAL**: The backbone pattern in `core` (traits, types, managers) is solid and must NOT be broken. All new features and exchanges must conform to the existing trait interfaces and type system.

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

### REST Response Parsing (`ResponseExt`)
All order-critical REST calls MUST use the `ResponseExt` trait from `core/src/http/client.rs`, NOT bare `.json()`. This ensures raw response bodies are always logged on parse failure.

- **`resp.json_logged(context)`** — Reads body as text, parses JSON. Logs full raw body at ERROR on failure. Use for standard API calls.
- **`check_and_parse(resp, context, is_error)`** — Free async function. Reads body, runs `fn(&str) -> Option<String>` error detector (catches HTTP 200 with error payload like Kraken/Binance edge cases), then parses JSON. Use for exchanges that embed errors in 200 responses.
- **`resp.text_logged(context)`** — Returns `(body, StatusCode)` for full manual control.

Design note: `check_and_parse` is a free function (not on the trait) because `async_trait` has lifetime issues with `fn(&str)` pointer parameters.

When adding a new exchange connector, always use `json_logged` for order endpoints at minimum. If the exchange is known to return 200 with error payloads, write an `fn(&str) -> Option<String>` detector and use `check_and_parse`.

### Data Persistence Strategy
- **Service Discovery**: Redis is currently used primarily for ZMQ Service Discovery (`core/src/zmq/discovery.rs`).
- **State Storage**: Application state (positions, orders, balances) is currently kept in-memory within the Actor managers.
- **Future Persistence**: When implementing persistence, structure Redis keys hierarchically: `{category}:{exchange}:{symbol}`. Use sorted sets for time-series data (funding rate history) and hashes for structured data. Treat exchanges as the source of truth, not local storage.

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
- **Domain/Critical Errors**: Use explicit `thiserror` enums for core trading operations, risk management, state transitions, and configuration validation. Never swallow these; propagate to top-level.
- **Transient/Network Errors**: Use `anyhow::Result` for HTTP failures, WebSocket disconnects, and retryable operations. Always use `.context()` when converting library errors.
- **Panics**: Only panic on programming errors (e.g., mutex poisoning), never on runtime conditions.
- **Logging**: Log critical errors at ERROR level, retryable at WARN, recoverable at INFO.

### Concurrency & State Management
- **Actor Model**: The `core/manager/*` module uses an Actor-based design (`AccountManagerActor`, `PositionManagerActor`, etc.). State is kept in standard `HashMap`s encapsulated *inside* these actors.
- **Message Passing**: Access actor state asynchronously via `mpsc` commands and `oneshot` replies. Do NOT use shared mutable state (like `DashMap` or `RwLock`) to bypass the actor mailboxes.
- **Async Runtime**: Use `tokio`. Spawn separate tasks for each exchange connector and actor.
- **Event Streams**: Use `tokio::sync::broadcast` or `tokio::sync::mpsc` for streams of events (e.g., order updates, individual trades).
- **Global Single Values**: Use `tokio::sync::watch` ONLY for single, infrequently changing global values (e.g., `ExecutionMode`, `IndexPrice`, system status).

### Inter-Process Communication (ZMQ)
- **Decentralized API**: Each exchange connector provides its own ZMQ API (Server: ROUTER/PUB) to expose necessary data.
- **Clients**: Strategies, risk management, quant research, and data lake integrations act as separate processes/clients (DEALER/SUB) connecting to the exchange's ZMQ server.
- **Protocol**: Communication uses the `ZmqCommand` and `ZmqEvent` protocol defined in `core/src/zmq/protocol.rs`.
- **Extensibility**: The current ZMQ API is designed to be easily extended in the future to support new data requirements for risk management and quantitative analysis.

### Orchestration & CLI
- **ExchangeRunner**: The `ExchangeRunner` (in `src/runner.rs`) is the central orchestrator for a specific exchange. It wires up the actors, exchange clients, and the ZMQ server.
- **CLI Tools**: The `src/cli/` module contains a `wizard` for generating configuration files (`local.toml`) and a `doctor` for health checks. New configuration parameters must be added to the wizard and validated by the doctor.

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
- Don't import inside functions, structs, or enums use `std::collections::HashSet` instead of `use std::collections::HashSet;`
- Don't use bare `.json()` on `reqwest::Response` for order-critical endpoints — use `ResponseExt::json_logged()` or `check_and_parse()` so raw bodies are always logged on failure

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

## current module structure
- `core/`: Shared abstractions, traits, types, managers (account, order, position, price), HTTP/WS utilities (`ResponseExt`, `check_and_parse`), and ZMQ protocol. **(SOLID BACKBONE - DO NOT BREAK)**
- `exchanges/`: Exchange-specific implementations (e.g., `binance/`). Must implement `core` traits.
- `strategies/`: Trading strategies (e.g., `arbitrage/`, `manual/`) and REPL.
- `src/`: Main entry points, CLI, wizard, and runner.
- `config/`: Configuration files (`default.toml`, `local.toml`, `exchanges/`, `strategies/`).

## project goals
1. Build a robust, modular architecture that supports multiple exchanges with minimal code duplication
2. Implement a comprehensive risk management framework with configurable parameters
3. Enable seamless switching between live and paper trading modes for testing and validation
4. Ensure consistent error handling with explicit enums for critical paths and anyhow for retryable errors
5. Create a unified type system for all exchange interactions to simplify integration and reduce bugs
6. create different strategies. future Vs Spot, future Vs future, delta neutral, volatility based, etc
7. must have internal api to query current positions, funding rates, execution, market data to support strategy, Risk management, data lake and quantitative trading research