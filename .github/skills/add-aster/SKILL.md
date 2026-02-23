# Add Aster Exchange Skill

## Description
Use this skill when implementing the Aster V3 API connector for the `boldtrax-bot` system. It provides the necessary context, base URLs, authentication rules, and documentation links required to build the integration correctly.

## Keywords
add aster, implement aster, aster v3, aster api, aster connector, asterdex

## Instructions

When implementing the Aster connector, you MUST follow the general rules in the `add-exchange` skill, plus these Aster-specific requirements:

### 1. Base Configuration
- **REST API Base URL**: `https://fapi.asterdex.com`
- **WebSocket Base URL**: `wss://fstream.asterdex.com`
- **Mandatory Config Fields**: The `AsterConfig` struct in `core/src/config/exchanges/aster.rs` MUST include:
  - `api_key` (String)
  - `api_secret` (String)
  - `recv_window` (u64, default to 5000)

### 2. API Version & Similarity
- You MUST use **API V3**.
- **CRITICAL**: Aster V3's payload structure and endpoint design are heavily inspired by Binance USDⓈ-M Futures. You can often reuse similar mapping logic and parameter names (e.g., `/fapi/v3/order`, `recvWindow`, `timeInForce`, `positionSide`).

### 3. Authentication (API Key)
Aster V3 uses a standard API Key authentication system, similar to Binance:
- **Headers Required**:
  - `X-MBX-APIKEY`: Your API key
- **Signature Generation**:
  - The signature is generated using HMAC SHA256 with the API Secret.
  - The string to sign is the query string (for GET) or the request body (for POST).
  - The signature is appended to the request as a parameter: `&signature=...`

### 4. Documentation Fetching
Do NOT guess the endpoint schemas. Use the `github_repo` tool to read the exact JSON structures from the official documentation repository before implementing the mappers:

- **Repository**: `asterdex/api-docs`
- **File**: `aster-finance-futures-api-v3.md`
- **Query**: Search for specific endpoints like `/fapi/v3/ticker/price`, `/fapi/v3/fundingRate`, `/fapi/v3/account`, `/fapi/v3/order`, and `/fapi/v3/positionRisk`.

### 5. Implementation Quirks
- Like Binance, Aster returns numbers as strings in JSON (e.g., `"price": "50000.0"`). Ensure your `serde` structs use `rust_decimal::Decimal` and handle string deserialization correctly (e.g., `#[serde(with = "rust_decimal::serde::str")]`).
- Pay close attention to the `positionSide` parameter (e.g., `BOTH`, `LONG`, `SHORT`) when placing orders and managing positions, as Aster supports hedge mode.
- Ensure you handle the `recvWindow` parameter correctly to prevent request expiration errors.