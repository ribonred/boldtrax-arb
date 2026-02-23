# Add New Exchange Skill

## Description
Use this skill when adding a new cryptocurrency exchange connector to the `boldtrax-bot` system. It outlines the strict architectural requirements, trait implementations, and error handling patterns required for a new exchange integration.

## Keywords
add exchange, new exchange, exchange connector, implement exchange, binance, bybit, okx, kraken, coinbase

## Instructions

When adding a new exchange to the system, you MUST follow these steps to maintain the solid core architecture:

1. **Module Structure**: Create a new module under `exchanges/src/[exchange_name]/`.
2. **Configuration Driven**: The exchange must be configuration-driven. Start by defining the configuration structure in `core/src/config/exchanges/` and updating the CLI wizard (`src/cli/wizard/`).
3. **Implementation Order**: Implement the exchange in this specific order:
   - **Configuration**: Define the config struct and validation.
   - **Client**: Implement the base HTTP/WS client with authentication.
   - **Public API**: Implement the `MarketData` trait (orderbook, funding rates, tickers).
   - **Private API**: Implement the `Account` and `Trading` traits (balances, orders, positions).
4. **Required Traits**: Implement all required traits from `core/src/traits.rs`:
   - `MarketData`
   - `Account`
   - `Trading`
   - `Exchange` (the combined trait)
5. **Type Isolation**: 
   - Create exchange-specific types in `exchanges/src/[exchange_name]/types.rs`.
   - NEVER expose exchange-specific types outside its module boundary.
   - Convert all exchange responses to the common core types defined in `core/src/types.rs` before returning.
6. **Metrics Integration**: Wrap all HTTP requests with `MetricsCollector::wrap_request()` to record exchange name, endpoint, duration, and success status.
7. **WebSocket Handler**: Implement the `WebSocketHandler` trait if the exchange supports WebSockets. Parse exchange-specific formats internally and emit standard `WsMessage` types.
8. **Error Handling**: 
   - Define exchange-specific error types in `exchanges/src/[exchange_name]/errors.rs` if the exchange has unique error conditions.
   - Use explicit `thiserror` enums for critical paths (authentication, insufficient balance, position conflicts).
   - Use `anyhow::Result` for retryable/transient network errors.
9. **Mock Implementation**: Create a mock version for testing if the exchange has unique features that need to be simulated in paper trading mode.

**CRITICAL**: The backbone pattern in `core` is solid and must NOT be broken. All new exchanges must conform to the existing trait interfaces and type system. Do not modify `core` traits to accommodate a single exchange's quirks; handle the quirks internally within the exchange module. except if necessary for a critical new feature that benefits all exchanges (in which case, follow the proper RFC process to update core traits).
