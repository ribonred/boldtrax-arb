# Add Hyperliquid Exchange Skill

## Description
Use this skill when implementing the Hyperliquid perpetuals connector for the `boldtrax-bot` system. It provides the necessary context, architecture differences, authentication rules, API endpoints, and mapping guidance to correctly integrate Hyperliquid into the existing trait-based exchange framework.

## Keywords
add hyperliquid, implement hyperliquid, hyperliquid api, hyperliquid perp, hl connector, hyperliquid integration

## Instructions

When implementing the Hyperliquid connector, you MUST follow the general rules in the `add-exchange` skill, plus these Hyperliquid-specific requirements. **Hyperliquid is fundamentally different from Binance/Aster/Bybit — do NOT reuse those patterns blindly.**

---

### 1. Base Configuration

- **REST Info Base URL**: `https://api.hyperliquid.xyz/info`
- **REST Exchange Base URL**: `https://api.hyperliquid.xyz/exchange`
- **WebSocket Base URL (Mainnet)**: `wss://api.hyperliquid.xyz/ws`
- **WebSocket Base URL (Testnet)**: `wss://api.hyperliquid-testnet.xyz/ws`
- **Testnet Info URL**: `https://api.hyperliquid-testnet.xyz/info`
- **Testnet Exchange URL**: `https://api.hyperliquid-testnet.xyz/exchange`

**Mandatory Config Fields** in `HyperliquidConfig`:
- `wallet_address` (String) — the EVM address (0x…) used as the account identity
- `private_key` (String) — the secp256k1 private key for EIP-712 signing (hex, no 0x prefix)
- `testnet` (bool, default false)
- `instruments` (Vec<String>) — InstrumentKey strings, e.g. `"BTCUSDT-HL-SWAP"`
- `recv_window` (u64, default 5000) — tolerance in ms for nonce staleness

**IMPORTANT**: Hyperliquid does NOT use API key + HMAC secret. Authentication is **EIP-712 Ethereum wallet signing**. The `wallet_address` serves as the account identifier. There is no `X-MBX-APIKEY` header.

---

### 2. API Architecture — Critical Differences from Binance/Aster

Hyperliquid's API is fundamentally different in two key areas:

#### A. Unified POST Endpoints (not path-based REST)
- **Public data** → `POST https://api.hyperliquid.xyz/info` with `Content-Type: application/json`
- **Private trading** → `POST https://api.hyperliquid.xyz/exchange` with `Content-Type: application/json`
- There are NO separate paths per operation. The `"type"` field in the JSON body routes the request.

**Info endpoint examples:**
```json
// Funding rate snapshot
{"type": "metaAndAssetCtxs"}

// User positions
{"type": "clearinghouseState", "user": "0xWalletAddress"}

// Order book (5 levels)
{"type": "l2Book", "coin": "BTC", "nSigFigs": 5}

// Funding history
{"type": "fundingHistory", "coin": "BTC", "startTime": 1234567890000}

// Exchange metadata (instruments)
{"type": "meta"}

// User open orders
{"type": "openOrders", "user": "0xWalletAddress"}
```

#### B. EIP-712 Signed Actions (not HMAC)
All mutating requests to `/exchange` require an EIP-712 signature over the action payload.
There is NO `apply_binance_hmac_auth`. You must implement a `HyperliquidSigner` that:

1. Constructs the action struct (e.g., order, cancel, updateLeverage)
2. Signs with secp256k1 using EIP-712 via the `alloy` or `k256` crate
3. Attaches `{ "action": ..., "nonce": <ms_timestamp>, "signature": {"r": "0x...", "s": "0x...", "v": <27|28>} }`

---

### 3. Authentication — EIP-712 Signing

Create `exchanges/src/hyperliquid/auth.rs` with a `HyperliquidSigner` struct.
This is NOT an `AuthProvider` — signing happens at the action level, not at the HTTP request level.

```rust
pub struct HyperliquidSigner {
    pub wallet_address: String,   // "0x..." lowercase hex
    pub private_key_hex: String,  // 32-byte hex, no 0x prefix
}

impl HyperliquidSigner {
    /// Sign a Hyperliquid action and return the full exchange request body as serde_json::Value.
    pub fn sign_action(
        &self,
        action: serde_json::Value,
        nonce: u64,
        vault_address: Option<&str>,
    ) -> anyhow::Result<serde_json::Value>;
}
```

The EIP-712 domain for Hyperliquid order actions:
```json
{
  "name": "HyperliquidSignTransaction",
  "version": "1",
  "chainId": 1337,
  "verifyingContract": "0x0000000000000000000000000000000000000000"
}
```

Use `alloy` crate for EIP-712 signing (preferred). Add to `exchanges/Cargo.toml`:
```toml
alloy = { version = "0.x", features = ["signers", "signer-local"] }
```

Fetch exact EIP-712 struct definitions from:
`https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/exchange-endpoint`

#### WebSocket Private Subscriptions
For user data (`orderUpdates`), subscriptions include `"user": "0xWalletAddress"` in the subscription message — no listenKey mechanism exists. The WS connection does NOT require authentication at connect time.

```json
{"method": "subscribe", "subscription": {"type": "orderUpdates", "user": "0xWalletAddress"}}
```

---

### 4. Documentation Fetching

Do NOT guess schema structures. Fetch these official documentation pages before implementing:

- **Info endpoints**: `https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint/perpetuals`
- **Exchange endpoint**: `https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/exchange-endpoint`
- **WebSocket subscriptions**: `https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/websocket/subscriptions`
- **WebSocket post requests**: `https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/websocket/post-requests`

---

### 5. Module File Structure

Create these files under `exchanges/src/hyperliquid/`:

```
exchanges/src/hyperliquid/
├── mod.rs          — re-exports, public API
├── auth.rs         — HyperliquidSigner (EIP-712 signing helper, NOT AuthProvider)
├── client.rs       — HyperliquidClient + all trait impls
├── types.rs        — HL-specific serde types (info/exchange responses)
├── mappers.rs      — HL response → core types conversions
├── ws.rs           — HyperliquidDepthPolicy + HyperliquidUserDataPolicy
└── codec.rs        — ClientOrderId encoding (128-bit hex cloid)
```

Register in `exchanges/src/lib.rs`:
```rust
pub mod hyperliquid;
```

---

### 6. Instrument Loading — `metaAndAssetCtxs` (REQUIRED, not `meta`)

**CRITICAL**: Use `metaAndAssetCtxs` (not `meta`) for instrument loading because `tick_size` must be derived from the live `markPx` field. Hyperliquid enforces strict decimal precision — using a wrong tick_size will cause order rejections.

```json
POST /info  {"type": "metaAndAssetCtxs"}
```

Returns a 2-element JSON array:
```json
[
  {                          // index 0: metadata
    "universe": [
      {
        "name": "BTC",       // coin name (NOT "BTCUSDT") — this IS exchange_symbol
        "szDecimals": 5,     // drives lot_size calculation
        "maxLeverage": 50,
        "marginMode": "cross"
      }
    ]
  },
  [                          // index 1: per-asset contexts (same order as universe)
    {
      "markPx": "65000.0",   // used to compute tick_size
      "midPx": "65001.5",
      "oraclePx": "64999.0",
      "funding": "0.0001",
      "openInterest": "1234.5",
      "impactPxs": ["64999.0", "65001.0"],
      "premium": "0.00005",
      "dayNtlVlm": "999999.0",
      "prevDayPx": "64500.0"
    }
  ]
]
```

#### `lot_size` and `min_notional` — from `szDecimals`

`szDecimals` defines **the step size for order quantity** (how many decimal places the size field may have).
It also implies the minimum order size — the smallest nonzero quantity is exactly `10^(-szDecimals)`.
Hyperliquid enforces a minimum notional of **$10 USDC** across all perpetuals (fixed, not per-instrument).

```rust
// lot_size (step size) = 10^(-sz_decimals)
// e.g. szDecimals=5 → lot_size = 0.00001  (BTC min increment)
//      szDecimals=2 → lot_size = 0.01     (SOL min increment)
let lot_size = Decimal::new(1, meta.sz_decimals);

// min_notional = $10 USDC (fixed for all HL perps)
// Set on the Instrument as: min_notional = Some(Decimal::from(10))
let min_notional = Some(Decimal::from(10u64));

// contract_size = 1 (linear perpetuals, 1 unit of base asset per contract)
let contract_size = Decimal::ONE;
```

**What szDecimals controls:**
- `lot_size` (quantity step) = `10^(-szDecimals)`  
- Minimum valid order size = `1 × lot_size` (one step)  
- Price and size in REST/WS order fields must have ≤ `szDecimals` decimal places  
- Does NOT control price precision — that comes from `markPx` via `compute_tick_size()`

**Examples:**
| Coin | szDecimals | lot_size  | min_qty   | min_notional |
|------|------------|-----------|-----------|--------------|
| BTC  | 5          | 0.00001   | 0.00001   | $10 USDC     |
| ETH  | 4          | 0.0001    | 0.0001    | $10 USDC     |
| SOL  | 2          | 0.01      | 0.01      | $10 USDC     |
| XRP  | 1          | 0.1       | 0.1       | $10 USDC     |

#### `tick_size` — dynamically computed from `markPx` (NEVER use a hardcoded default)
Hyperliquid uses **5 significant figures** for perp prices, capped at `maxDecimals=6` total decimal places.

```rust
/// Compute Hyperliquid perp tick_size from a live mark price and szDecimals.
/// Mirrors CCXT's calculatePricePrecision(price, amountPrecision, maxDecimals=6).
pub fn compute_tick_size(mark_px: Decimal, sz_decimals: u32) -> Decimal {
    const MAX_DECIMALS: u32 = 6;
    const SIG_FIGS: u32 = 5;

    if mark_px.is_zero() {
        // Fallback: 5 sig figs minus amount precision
        return Decimal::new(1, SIG_FIGS.saturating_sub(sz_decimals));
    }

    let price_f64 = mark_px.to_f64().unwrap_or(1.0);
    let price_decimals: u32 = if price_f64 >= 1.0 {
        // e.g. BTC=65000 → floor(log10(65000))=4 integer digits
        // sig_figs=max(5,4)=5, price_decimals = 5-5 = 0... but clamp to >=0
        let int_digits = price_f64.log10().floor() as u32 + 1;
        let sig = SIG_FIGS.max(int_digits);
        sig.saturating_sub(int_digits)
    } else {
        // price < 1: count leading zeros after decimal, add sig figs
        let leading_zeros = (-price_f64.log10().ceil()) as u32;
        (leading_zeros + SIG_FIGS).min(MAX_DECIMALS - sz_decimals)
    };

    // Cap at maxDecimals - sz_decimals
    let decimals = price_decimals.min(MAX_DECIMALS.saturating_sub(sz_decimals));
    Decimal::new(1, decimals)
}
```

**Examples:**
| Coin | markPx | szDecimals | tick_size | lot_size |
|------|--------|------------|-----------|----------|
| BTC  | 65000  | 3          | 0.1       | 0.001    |
| ETH  | 3500   | 4          | 0.01      | 0.0001   |
| SOL  | 150    | 2          | 0.001     | 0.01     |
| XRP  | 0.55   | 1          | 0.00001   | 0.1      |

#### Pairs and quote currency
All Hyperliquid perpetuals are quoted in USDC. Map coin names to `Pairs` enum:
- `"BTC"` → `Pairs::BTCUSDC` (quote is always USDC)
- `"ETH"` → `Pairs::ETHUSDC`
- etc.



#### `HyperliquidSwapMeta` in `types.rs`
```rust
pub struct HyperliquidSwapMeta {
    pub coin: String,           // e.g. "BTC" — this becomes exchange_symbol
    pub asset_index: u32,       // position in universe array (0-indexed)
    pub sz_decimals: u32,
    pub tick_size: Decimal,     // computed via compute_tick_size(mark_px, sz_decimals)
    pub lot_size: Decimal,      // = Decimal::new(1, sz_decimals) — quantity step size
    pub min_notional: Decimal,  // always Decimal::from(10) — $10 USDC fixed minimum
    pub max_leverage: u32,
    pub funding_rate: Decimal,
    pub mark_px: Decimal,
    pub oracle_px: Decimal,
    pub pairs: Pairs,
    pub base_currency: Currency,
}
```

When converting `HyperliquidSwapMeta` → `Instrument`, populate:
```rust
Instrument {
    key: InstrumentKey { exchange: Exchange::Hyperliquid, pair: meta.pairs, instrument_type: InstrumentType::Swap },
    exchange_symbol: meta.coin.clone(),            // "BTC", "ETH", etc.
    exchange_id: meta.asset_index.to_string(),     // "0", "1", "2" — asset index as string
    tick_size: meta.tick_size,                     // from compute_tick_size(mark_px, sz_decimals)
    lot_size: meta.lot_size,                       // from Decimal::new(1, sz_decimals)
    min_notional: Some(meta.min_notional),         // always Some(Decimal::from(10))
    contract_size: Some(Decimal::ONE),             // linear perp, 1 unit per contract
    multiplier: Decimal::ONE,
    funding_interval: Some(FundingInterval::EveryHour),
}
```

**`exchange_id` stores the asset index as a string** (`"0"`, `"1"`, `"2"`, …). This is the source of truth for the `"a"` field in all order/cancel/leverage actions — parse it back with `.parse::<u32>()` directly from the registry-returned `Instrument`. No separate `HashMap<InstrumentKey, u32>` is needed.

```rust
// In place_order, cancel_order, set_leverage — retrieve asset index from registry:
let instrument = self.registry.get(&key).ok_or(TradingError::UnsupportedInstrument)?;
let asset_index: u32 = instrument.exchange_id.parse()
    .map_err(|_| TradingError::Other(format!("invalid asset index in exchange_id: {}", instrument.exchange_id)))?;
```

---

### 7. Funding Rate Data — from `metaAndAssetCtxs`

Since `load_instruments()` already calls `metaAndAssetCtxs`, the initial funding snapshot is available for free from the same response. For a fresh snapshot call:

```json
POST /info  {"type": "metaAndAssetCtxs"}
```

Match by array index: `universe[i].name` corresponds to `asset_contexts[i]`.

Map to `FundingRateSnapshot`:
- `funding_rate` = `asset_ctx.funding` parsed as Decimal
- `mark_price` = `asset_ctx.markPx`
- `index_price` = `asset_ctx.oraclePx`
- `next_funding_time` = compute next top-of-hour from `Utc::now()` (Hyperliquid funds every 1h at the top of the hour)
- `interval` = `FundingInterval::EveryHour`

For `funding_rate_history`:
```json
POST /info  {"type": "fundingHistory", "coin": "BTC", "startTime": <ms>, "endTime": <ms>}
```
Response: `[{"coin": "BTC", "fundingRate": "0.0001", "premium": "0.00005", "time": 1234567890000}]`

---

### 8. Order Book — `l2Book`

```json
POST /info  {"type": "l2Book", "coin": "BTC", "nSigFigs": 5}
```

Response:
```json
{
  "coin": "BTC",
  "time": 1234567890000,
  "levels": [
    [{"px": "65000.0", "sz": "1.5", "n": 3}, ...],  // index 0 = bids
    [{"px": "65001.0", "sz": "0.8", "n": 2}, ...]   // index 1 = asks
  ]
}
```

Map `px` → `price`, `sz` → `quantity`. Take up to 5 levels per side.

---

### 9. Order Placement — Signed Action

**Place Order** (`POST /exchange`):

Build the action, then call `HyperliquidSigner::sign_action()` to produce the full request body:

```json
{
  "action": {
    "type": "order",
    "orders": [{
      "a": 0,             // asset_index = instrument.exchange_id.parse::<u32>()
      "b": true,          // isBuy
      "p": "65000.0",     // price string; use "0" as placeholder and Ioc for market
      "s": "0.001",       // size string
      "r": false,         // reduceOnly
      "t": {"limit": {"tif": "Gtc"}},   // Alo=post-only, Ioc=market-like, Gtc=standard
      "c": "0x00000000000000000000000000000001"  // optional 128-bit hex cloid
    }],
    "grouping": "na"
  },
  "nonce": 1700000000000,
  "signature": {"r": "0x...", "s": "0x...", "v": 28}
}
```

**Market orders**: Use `"t": {"limit": {"tif": "Ioc"}}` with price set to slippage-adjusted value.

**Order response** (resting):
```json
{"status": "ok", "response": {"type": "order", "data": {"statuses": [{"resting": {"oid": 123456}}]}}}
```
**Order response** (filled):
```json
{"status": "ok", "response": {"type": "order", "data": {"statuses": [{"filled": {"totalSz": "0.001", "avgPx": "65000.0", "oid": 123456}}]}}}
```

**Cancel Order** (`POST /exchange`):
```json
{
  "action": {
    "type": "cancel",
    "cancels": [{"a": 0, "o": 123456}]   // asset index + oid (integer)
  },
  "nonce": ...,
  "signature": ...
}
```

Note: `cancel` uses the exchange `oid` (integer), not the cloid string.

**ClientOrderId (cloid)** in `codec.rs`: Hyperliquid supports a 128-bit hex cloid (`"0x" + 32 hex chars`).
```rust
pub struct HyperliquidClientOrderIdCodec;
// encode: format as "0x{:032x}" from a hash of internal_id
// The cloid is optional; if omitted, use the returned oid for cancel operations
```

---

### 10. Account / Positions — `clearinghouseState`

```json
POST /info  {"type": "clearinghouseState", "user": "0xWalletAddress"}
```

Response:
```json
{
  "assetPositions": [
    {
      "position": {
        "coin": "BTC",
        "szi": "0.5",              // signed size (positive=long, negative=short)
        "entryPx": "64000.0",
        "leverage": {"type": "cross", "value": 10},
        "liquidationPx": "60000.0",
        "unrealizedPnl": "500.0",
        "marginUsed": "3200.0"
      },
      "type": "oneWay"
    }
  ],
  "marginSummary": {
    "accountValue": "10000.0",
    "totalMarginUsed": "3200.0",
    "totalNtlPos": "32000.0",
    "totalRawUsd": "9500.0",
    "withdrawable": "6800.0"
  }
}
```

Map positions: `szi` → `size` (signed), `entryPx` → `entry_price`, `unrealizedPnl` → `unrealized_pnl`, `liquidationPx` → `liquidation_price`, `leverage.value` → `leverage`.

For `AccountSnapshot`:
- `asset` = `Currency::USDC` (Hyperliquid collateral)
- `total` = `marginSummary.accountValue`
- `free` = `marginSummary.withdrawable`
- `locked` = `total - free`
- `AccountModel::Unified` (single collateral pool)
- Use `PartitionKind::Swap` with `Exchange::Hyperliquid`

---

### 11. WebSocket Handlers

#### Public — Order Book Streaming (`HyperliquidDepthPolicy`)
```json
// Subscribe after connect
{"method": "subscribe", "subscription": {"type": "l2Book", "coin": "BTC"}}

// Inbound message format
{
  "channel": "l2Book",
  "data": {
    "coin": "BTC",
    "time": 1234567890000,
    "levels": [
      [{"px": "65000.0", "sz": "1.5", "n": 3}],
      [{"px": "65001.0", "sz": "0.8", "n": 2}]
    ]
  }
}
```

In `HyperliquidDepthPolicy`:
- `prepare()` → returns WS URL; store subscription messages to send
- Override a post-connect hook or send subscriptions at start of first `parse_message` call
- `parse_message()` → parse `channel == "l2Book"`, build `OrderBookUpdate`

**Key difference from Aster**: Subscriptions are sent as WS messages after connect, not encoded in the URL path. The `WsPolicy::prepare()` method should return the WS URL, and subscriptions should be sent as the first messages after connection.

#### Private — User Order Updates (`HyperliquidUserDataPolicy`)
```json
// Subscribe after connect
{"method": "subscribe", "subscription": {"type": "orderUpdates", "user": "0xWalletAddress"}}

// Inbound order update
{
  "channel": "orderUpdates",
  "data": [{
    "order": {
      "coin": "BTC",
      "side": "B",            // "B"=buy, "A"=ask/sell
      "limitPx": "65000.0",
      "sz": "0.001",
      "oid": 123456,
      "timestamp": 1700000000000,
      "cloid": "0x..."
    },
    "status": "open",         // "open", "filled", "canceled", "triggered"
    "statusTimestamp": 1700000000001
  }]
}
```

Map `status` to `OrderStatus`:
- `"open"` → `OrderStatus::New`
- `"filled"` → `OrderStatus::Filled`
- `"canceled"` → `OrderStatus::Canceled`

**No heartbeat needed** — Hyperliquid WS does not require keepalive messages.
**No listenKey** — subscribe directly with `user` field.
**No `prepare()` REST call** — just return WS URL.

---

### 12. Leverage Setting

```json
POST /exchange  (signed action)
{
  "action": {
    "type": "updateLeverage",
    "asset": 0,         // asset index
    "isCross": true,    // true=cross margin, false=isolated
    "leverage": 10
  },
  "nonce": ...,
  "signature": ...
}
```

Response: `{"status": "ok", "response": {"type": "default"}}`

Return the requested leverage as the actual leverage (Hyperliquid confirms via status "ok").

---

### 13. Config & CLI Wizard

Create `src/cli/wizard/exchanges/hyperliquid.rs`:

```rust
pub struct HyperliquidWizard;

impl ExchangeWizard for HyperliquidWizard {
    fn exchange(&self) -> Exchange { Exchange::Hyperliquid }

    fn prompt(&self) -> anyhow::Result<toml::Value> {
        // Prompt for: testnet (bool), wallet_address (string), private_key (password/secret)
        // DO NOT prompt for api_key/api_secret — those don't exist on Hyperliquid
        // Display security warning: private_key has full signing authority
    }
}
```

Register `HyperliquidWizard` in `src/cli/wizard/exchanges/mod.rs` alongside Aster/Bybit.

---

### 14. Key Implementation Quirks & Gotchas

1. **Coin vs Symbol**: Hyperliquid uses short coin names (`"BTC"`, `"ETH"`, `"SOL"`) not `"BTCUSDT"`. The `exchange_symbol` on `Instrument` will be the coin name. Registry lookups via `get_by_exchange_symbol` use the coin name.

2. **Numbers as strings**: Hyperliquid returns most numeric values as JSON strings. Use `#[serde(with = "rust_decimal::serde::str")]` or `String` deserialization then parse. Verify each field from docs — some may be plain floats.

3. **Asset index from `exchange_id`**: Unlike Binance (symbol string), Hyperliquid requires the integer asset index (`"a": 0`). Store it as `exchange_id` on `Instrument` (e.g. `"0"`, `"1"`) during `load_instruments()`. Retrieve with `instrument.exchange_id.parse::<u32>()`. No separate HashMap needed.

4. **Signed position size**: `szi` is signed (positive=long, negative=short). Preserve sign in `Position.size`.

5. **Single USDC collateral**: One margin pool. `AccountSnapshot` uses `AccountModel::Unified` with one `BalanceView` for `Currency::USDC`.

6. **Perpetuals only**: No spot instruments on Hyperliquid perps API. Return `MarketDataError::UnsupportedInstrument` for `InstrumentType::Spot`.

7. **EIP-712 chain ID**: `1337` for mainnet, `421614` for testnet — verify against official docs before implementing.

8. **Nonce freshness**: Nonce = millisecond timestamp. Always generate a fresh nonce per request; do not reuse.

9. **WS re-subscription on reconnect**: On every reconnect, `HyperliquidDepthPolicy` must resend all `l2Book` subscription messages. `prepare()` is called on each reconnect by the supervisor.

10. **Order status from REST vs WS**: REST `openOrders` returns live orders; WS `orderUpdates` provides real-time updates. Implement `stream_executions` via WS.

---

### 15. Implementation Order (follow this sequence)

1. `types.rs` — Define all Hyperliquid serde structs (info responses, exchange responses, WS messages)
2. `auth.rs` — Implement `HyperliquidSigner` with EIP-712 signing
3. `codec.rs` — Implement `HyperliquidClientOrderIdCodec` (128-bit hex cloid)
4. `mappers.rs` — Implement all response → core type conversions
5. `ws.rs` — Implement `HyperliquidDepthPolicy` and `HyperliquidUserDataPolicy`
6. `client.rs` — Implement `HyperliquidClient` with all trait impls
7. `mod.rs` — Re-export `HyperliquidClient` and `HyperliquidConfig`
8. `exchanges/src/lib.rs` — Add `pub mod hyperliquid;`
9. Verify `Exchange::Hyperliquid` with short code `"HL"` is already in `core/src/types.rs` (it is)
10. `src/cli/wizard/exchanges/hyperliquid.rs` + register in wizard mod
11. `src/main.rs` — Wire up `HyperliquidClient` in the exchange factory/runner
