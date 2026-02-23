# Add Bybit Exchange Skill

## Description
Use this skill when implementing the Bybit V5 API connector for the `boldtrax-bot` system. It provides the necessary context, base URLs, authentication rules, and documentation links required to build the integration correctly.

## Keywords
add bybit, implement bybit, bybit v5, bybit api, bybit connector

## Instructions

When implementing the Bybit connector, you MUST follow the general rules in the `add-exchange` skill, plus these Bybit-specific requirements:

### 1. Base Configuration
- **REST API Base URL**: `https://api.bybit.com` (or `https://api.bytick.com` as fallback)
- **WebSocket Base URL (Public)**: `wss://stream.bybit.com/v5/public/linear`
- **WebSocket Base URL (Private)**: `wss://stream.bybit.com/v5/private`
- **Mandatory Config Fields**: The `BybitConfig` struct in `core/src/config/exchanges/bybit.rs` MUST include:
  - `api_key` (String)
  - `api_secret` (String)
  - `account_type` (String, default to "UNIFIED")
  - `recv_window` (u64, default to 5000)

### 2. API Version & Category
- You MUST use **API V5**.
- For funding rate arbitrage, we are trading USDT/USDC Perpetuals. You MUST include `category="linear"` in all relevant requests (both REST and WebSocket).

### 3. Authentication (HMAC SHA256)
Bybit V5 requires a specific signature format for private endpoints:
- **Headers Required**:
  - `X-BAPI-API-KEY`: Your API key
  - `X-BAPI-TIMESTAMP`: Current timestamp in milliseconds
  - `X-BAPI-RECV-WINDOW`: Usually `5000`
  - `X-BAPI-SIGN`: The generated signature
- **Signature Generation**:
  - The string to sign is: `timestamp + api_key + recv_window + payload`
  - For GET requests, `payload` is the query string (e.g., `category=linear&symbol=BTCUSDT`).
  - For POST requests, `payload` is the raw JSON body.
  - Hash the string using HMAC SHA256 with the API Secret.

### 4. Documentation Fetching
Do NOT guess the endpoint schemas. Use the `fetch_webpage` tool to read the exact JSON structures from the official documentation before implementing the mappers:

- **Market Data (Tickers/Funding Rates)**: Fetch `https://bybit-exchange.github.io/docs/v5/market/tickers` and `https://bybit-exchange.github.io/docs/v5/market/funding-rate`
- **Account (Balances)**: Fetch `https://bybit-exchange.github.io/docs/v5/account/wallet-balance`
- **Trading (Orders)**: Fetch `https://bybit-exchange.github.io/docs/v5/order/create-order`
- **Positions**: Fetch `https://bybit-exchange.github.io/docs/v5/position`

### 5. Implementation Quirks
- Bybit returns numbers as strings in JSON (e.g., `"qty": "1.5"`). Ensure your `serde` structs use `rust_decimal::Decimal` and handle string deserialization correctly (e.g., `#[serde(with = "rust_decimal::serde::str")]`).
- The unified margin system means balances might be shared across spot and futures. Ensure you are querying the correct account type (`accountType="UNIFIED"` or `"CONTRACT"` depending on user setup, make this configurable).