# Strategies Module

## Overview

This module implements automated arbitrage strategies for the boldtrax-bot system.
All strategies are composed from pluggable policy traits (decision, execution,
margin, price) wired through a generic `ArbitrageEngine<D, E, M, P>`.

---

## Strategy: Spot-Perp Funding Rate Arbitrage

### Objective

Capture the **perpetual funding rate** by holding a delta-neutral position:

1. **Buy spot** (the underlying asset) — tracked via account balance, NOT a position.
2. **Short the perpetual** — tracked via a Position with leverage, liquidation price, etc.
3. When the perp funding rate is positive, **shorts receive funding from longs**.
   We earn that payment while "hedged" by our spot holding.

### Key Invariants

| Property | Spot Leg | Perp Leg |
|---|---|---|
| **InstrumentType** | `Spot` | `Swap` |
| **Funding rate** | Does NOT exist | Exists — drives entry/exit |
| **Position tracking** | Account balance (`quantity * price`) | `Position` struct (`size`, `entry_price`, `liquidation_price`) |
| **Liquidation risk** | None | Yes — must monitor liquidation distance |
| **Delta contribution** | `+quantity × spot_price` (positive, we're long) | `position_size × mark_price` (negative when short) |
| **Price source** | OrderBook / Ticker for the Spot key | OrderBook / Ticker for the Swap key + FundingRate events |

### Decision Logic (RebalanceDecider)

- **Enter**: perp `funding_rate >= min_funding_threshold` while pair is `Inactive`.
  Buy spot + short perp for the target notional.
- **Exit**: perp `funding_rate < 0` while pair is `Active` (we'd start paying, not earning).
- **Rebalance**: when `|total_delta| > threshold` (delta drifted too far from zero).
  Computes a **correction delta** (`half_delta / price` per leg) — not the full target —
  so each rebalance nudges the pair back toward neutral without overshooting.
- **DoNothing**: otherwise.

### Delta Calculation

```
total_delta = spot.quantity × spot.current_price
            + perp.position_size × perp.current_price
```

For a perfectly hedged position: spot quantity positive, perp position_size negative,
`total_delta ≈ 0`. Positive means we're net-long, negative means net-short.

### Data Model

```
SpotLeg {
    key: InstrumentKey       // (exchange, pair, Spot)
    quantity: Decimal         // from account balance
    avg_cost: Decimal         // average entry price for spot buys
    current_price: Decimal    // latest mid-price from oracle
    best_bid: Option<Decimal> // from latest orderbook snapshot
    best_ask: Option<Decimal> // from latest orderbook snapshot
}

PerpLeg {
    key: InstrumentKey        // (exchange, pair, Swap)
    position_size: Decimal    // negative when short
    entry_price: Decimal      // from PositionUpdate
    current_price: Decimal    // mark/mid price from oracle
    funding_rate: Decimal     // from FundingRate event
    best_bid: Option<Decimal> // from latest orderbook snapshot
    best_ask: Option<Decimal> // from latest orderbook snapshot
}

SpotPerpPair {
    spot: SpotLeg
    perp: PerpLeg
    target_notional: Decimal  // target dollar exposure per side
    status: PairStatus        // Inactive | Active
    last_spot_partial_fill: Decimal // tracks incremental spot fills
}
```

### Event Routing in the Engine

| ZmqEvent | Spot Leg | Perp Leg |
|---|---|---|
| `FundingRate` | Seeds `spot.current_price` from `index_price` if zero | Update `perp.funding_rate`, seed `perp.current_price` from `mark_price`, trigger evaluation |
| `OrderBook` / `Ticker` | Update `spot.current_price` via oracle | Update `perp.current_price` via oracle |
| `PositionUpdate` | **Ignored** — spot uses account balance | Update `perp.position_size`, `perp.entry_price` |
| `AccountSnapshot` | Could be used to update `spot.quantity` | Margin checks on account-level leverage |
| `OrderUpdate` | **Track spot fills** — updates `spot.quantity` from partial + final fills | Log only |

### Margin Checks

- **Account-level**: leverage ratio check on the `AccountSnapshot` (applies to perp side).
- **Position-level**: liquidation distance check on the perp `Position` only.
  Spot has no liquidation — `SpotMarginPolicy` always returns `Ok`.

---

## Strategy: Perp-Perp Cross-Exchange (Future)

Long on one exchange, short on the other, capturing funding rate differential.
Both legs are `Swap` instruments. Both have funding rates, positions, and liquidation risk.
**Not yet implemented** — will use a different pair type (`PerpPerpPair`) with `spread() = funding_a - funding_b`.

---

## Architecture

### Policy Traits

All traits use a **template method** pattern: implementors supply `_inner` methods (pure logic),
the default wrapper adds structured `tracing` spans and `monotonic_counter` metrics.

| Trait | Purpose | Generic over pair? |
|---|---|---|
| `DecisionPolicy<P: PairState>` | `evaluate(pair) → DeciderAction` | Yes |
| `ExecutionPolicy<P: PairState>` | `execute(action, pair)` — places orders | Yes |
| `MarginPolicy` | `check_account` + `check_position` | No (uses `AccountSnapshot` + `Position`) |
| `PriceSource` | `update_snapshot` / `update_ticker` / `get_mid_price` | No (keyed by `InstrumentKey`) |

### PairState Trait

The `PairState` trait abstracts any tradable pair type, allowing the engine to be
fully pair-agnostic. Any pair struct (e.g. `SpotPerpPair`, future `PerpPerpPair`)
must implement:

| Method | Purpose |
|---|---|
| `total_delta()` | Net delta exposure across all legs |
| `status()` / `set_status()` | Current pair lifecycle state |
| `apply_event(ZmqEvent) → bool` | Route pair-specific events (e.g. FundingRate, PositionUpdate, OrderUpdate for spot fills). Returns `true` if evaluation should trigger. |
| `refresh_prices(oracle_fn)` | Update leg prices from the oracle |
| `refresh_orderbook(bid_fn, ask_fn)` | Update leg `best_bid`/`best_ask` from the oracle for smart order placement |
| `positions_for_margin_check()` | Enumerate positions needing margin validation |

### Engine

```
ArbitrageEngine<Pair: PairState, D: DecisionPolicy<Pair>, E: ExecutionPolicy<Pair>, M: MarginPolicy, P: PriceSource>
```

- Static dispatch, zero vtable overhead.
- Fully pair-agnostic — all pair-specific logic delegated via `PairState` trait.
- Uses `&mut self` methods — no argument explosion, struct stays intact.
- `run(subscriber)` — production entry point driven by ZMQ events.
- `run_with_stream(rx)` — test/simulation entry point driven by a channel.
  Returns `(Pair, E)` for assertions.

### Event Flow

```
ZmqEvent arrives
  ├─ OrderBook / Ticker / OrderBookUpdate → oracle (universal)
  ├─ OrderUpdate → pair.apply_event (spot fill tracking) + log
  └─ everything else → pair.apply_event(event)
       └─ if true → evaluate_and_execute()
            ├─ margin.check_account (gate)
            ├─ pair.refresh_prices (from oracle)
            ├─ pair.refresh_orderbook (bid/ask from oracle)
            ├─ margin.check_position (per-leg gate)
            └─ decider.evaluate → execution.execute
```

### Execution Strategy

The `ExecutionEngine` uses **instrument-aware order placement**:

| Leg | Order Type | Pricing | `post_only` | `reduce_only` |
|---|---|---|---|---|
| **Spot BUY** | `LIMIT_MAKER` | Best ask from orderbook | `true` | `false` (not supported) |
| **Spot SELL** | `LIMIT_MAKER` | Best bid from orderbook | `true` | `false` (not supported) |
| **Perp (enter/rebalance)** | `MARKET` | N/A | `false` | `false` |
| **Perp (exit)** | `MARKET` | N/A | `false` | `true` |

**Rationale**: Spot uses `LIMIT_MAKER` (Binance's post-only order type) to earn maker fee
rebates rather than paying taker fees. Falls back to `MARKET` if no orderbook data is available.
Perp uses `MARKET` for immediate fill certainty since the perp side has liquidation risk.

### REST Response Handling

All order-related REST calls use `ResponseExt::json_logged()` from `core/src/http/client.rs`.
This reads the response body as text first, then parses JSON. On parse failure the **full raw body**
is logged at ERROR level — we never silently lose exchange payloads.

Three tiers of response parsing are available:

| API | Use Case |
|---|---|
| `resp.json_logged(ctx)` | Simple parse + log raw body on failure |
| `check_and_parse(resp, ctx, detector)` | Run an `fn(&str) -> Option<String>` error detector before parsing — catches HTTP 200 with error payload (Kraken, some Binance edge cases) |
| `resp.text_logged(ctx)` | Returns `(body, status)` for full manual control |

### Binance-Specific: `newOrderRespType=RESULT`

Binance spot API defaults to `ACK` response type for `LIMIT_MAKER` orders, which only returns
`symbol`, `orderId`, `clientOrderId`, `transactTime` — missing the fields we need (`price`,
`origQty`, `executedQty`, `status`). We force `&newOrderRespType=RESULT` on all spot order
submissions so the full response is always returned.

### Order Manager Resilience

If the REST response fails to parse but the WebSocket has already confirmed the order
(status moved beyond `PendingSubmit` to `New`), the order manager does **not** reject the
order. It logs a WARN and keeps the order alive, relying on WS for further updates.
Only orders still in `PendingSubmit` state are rejected on REST failure.

### Paper Trading

`PaperExecution<E>` decorates any `ExecutionPolicy`, simulating instant fills
(status → `Active` on enter, `Inactive` on exit) without sending real orders.

### Runner

`StrategyRunner` enum seals all valid engine type combinations:

```
SpotPerpEngine       = ArbitrageEngine<SpotPerpPair, SpotRebalanceDecider, ExecutionEngine, MarginManager, PriceOracle>
SpotPerpPaperEngine  = ... same with PaperExecution<ExecutionEngine> ...
```

---

## What NOT to Confuse

- **Spot-Perp**: ONE funding rate (the perp's). Spot has no funding, no position, no liquidation.
- **Perp-Perp**: TWO funding rates. Both legs have positions and liquidation risk.
  These are entirely different data models and decision logic.
- `ArbitrageLeg` (old, removed) treated both legs identically with `funding_rate` and
  `position_size` — incorrect for spot-perp because spot has neither.
