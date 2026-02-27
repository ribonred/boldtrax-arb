# Strategies Module

## Overview

This module implements automated arbitrage strategies for the boldtrax-bot system.
All strategies are composed from pluggable policy traits (decision, execution,
margin, price) wired through either an event-driven `ArbitrageEngine` (spot-perp)
or a poll-based `PerpPerpPoller` (perp-perp).

Two strategy types are fully implemented:
- **Spot-Perp** — single-exchange, event-driven, captures perp funding rate
- **Perp-Perp** — cross-exchange, poll-based, captures funding rate differential

---

## Lifecycle States

Every pair follows this lifecycle managed by `PairStatus`:

```
Inactive ──► Entering ──► Active ──► Exiting ──► Inactive
                │              │          ▲
                ▼              ▼          │
            Recovering ──► Active    Unwinding ──► Inactive
```

| Status | Description |
|---|---|
| `Inactive` | No open position — waiting for entry signal |
| `Entering` | Entry orders placed, waiting for both legs to fill |
| `Active` | Both legs filled and hedged — normal operation |
| `Exiting` | Exit orders placed, market close in progress |
| `Recovering` | One leg orphaned — smart recovery in progress |
| `Unwinding` | Command-driven graceful close — chunked over multiple cycles |

Transitional statuses (`Entering`, `Exiting`, `Recovering`, `Unwinding`) block the
decider from emitting new actions. The engine handles retries internally.

---

## Strategy: Spot-Perp Funding Rate Arbitrage

### Objective

Capture the **perpetual funding rate** by holding a delta-neutral position:

1. **Buy spot** (tracked via account balance, NOT a position)
2. **Short the perpetual** (tracked via a `Position` with leverage + liquidation price)
3. When perp funding rate is positive, shorts receive funding from longs

### Decision Logic (`SpotRebalanceDecider`)

- **Enter**: perp `funding_rate >= min_funding_threshold` while `Inactive`
- **Exit**: perp `funding_rate < 0` while `Active`
- **Rebalance**: `|total_delta| > target_notional × rebalance_drift_pct / 100`
  — splits half-delta correction across both legs
- **DoNothing**: otherwise

### Delta Calculation

```
total_delta = spot.quantity × spot.current_price
            + perp.position_size × perp.current_price
```

### Data Model

| Component | Spot Leg | Perp Leg |
|---|---|---|
| InstrumentType | `Spot` | `Swap` |
| Funding rate | None | Drives entry/exit |
| Position tracking | Account balance (`quantity × price`) | `Position` struct |
| Liquidation risk | None | Yes — monitored |
| Price source | OrderBook / Ticker | OrderBook / Ticker + FundingRate |

### Execution

| Leg | Entry/Rebalance | Exit |
|---|---|---|
| Spot | `LIMIT_MAKER` (post_only) at best bid/ask | Market |
| Perp | Market | Market + `reduce_only` |

**Rationale**: Spot uses `LIMIT_MAKER` to earn maker fee rebates. Perp uses Market
for immediate fill certainty since it has liquidation risk.

---

## Strategy: Perp-Perp Cross-Exchange Arbitrage

### Objective

Capture the **funding rate differential** between two exchanges:

1. **Long perp** on one exchange (where funding rate is lower / we receive)
2. **Short perp** on another exchange (where funding rate is higher / we receive)
3. Earn from the spread between the two funding rates

### Decision Logic (`PerpPerpDecider`)

- **Enter**: `funding_spread >= min_spread_threshold` while `Inactive`.
  Detects one-legged orphan positions and recovers them on entry.
- **Exit**: spread drops below `exit_threshold` (or < 0 if unset) while `Active`
- **Rebalance**: `|total_delta| > target_notional × rebalance_drift_pct / 100`
- **DoNothing**: otherwise

Funding spread is directional based on `CarryDirection` (`Positive` or `Negative`).

### Data Model

Both legs are `PerpLeg` structs: instrument key, position size, entry price, mark price,
funding rate, best bid/ask, and top-3 depth. `FundingCache` provides staleness-aware
in-memory caching of funding rate snapshots.

### Execution

| Action | Order Type | Notes |
|---|---|---|
| Entry | Limit + post_only at best bid/ask | Maker fees on both legs |
| Rebalance | Cancel-replace, then Limit + post_only | Cancels stale orders first |
| Exit | Market + reduce_only | Immediate close |
| Recovery | Limit + post_only on missing leg | Smart recovery system |
| Unwind | Market + reduce_only (chunked) | Gradual close |

### Cancel-Replace Pattern

Before dispatching rebalance orders, the execution engine cancels all pending tracked
orders via `cancel_pending_orders()`. This prevents order accumulation when prior limit
orders haven't filled within the poll interval. Each cancelled order is also force-marked
terminal in the tracker.

---

## Safety Systems

### Smart Recovery (`recovery.rs`)

When one leg fails during entry (partial fill, rejection), the pair enters `Recovering`
status. The recovery system uses economic analysis to decide the best action:

| Rule | Condition | Decision |
|---|---|---|
| Time backstop | > 60s elapsed | **Abort** — close the filled leg |
| Large loss | PnL loss > 0.3% of notional | **Abort** |
| Profit capture | PnL profit > crossing cost (5 bps) | **Abort** (bank it) |
| Loss recovery | PnL loss > crossing cost | **Chase** (market order) |
| No resting order | Order tracker empty | **Reprice** (new limit) |
| Stale order | Order older than threshold | **Reprice** |
| Default | — | **Rest** (wait for fill) |

### Emergency Margin Exit

When a position's liquidation distance falls below the configured safety
buffer (`liquidation_buffer_pct`), the engine triggers an emergency exit:

1. **Cancel** all in-flight orders (free margin)
2. **Force exit** — market close both legs (`DeciderAction::Exit`)
3. **Pause** — strategy paused to prevent re-entry while account is at risk

This fires when the pair is `Active` or `Entering`. Transitional statuses
(`Exiting`, `Recovering`, `Unwinding`) already have their own close logic.
The engine remains paused until manually resumed via `stratctl resume`.

### Stale Order Cancellation

Orders pending longer than the configured timeout are auto-cancelled:
- **ArbitrageEngine** (spot-perp): 60s timeout, checked every 30s
- **PerpPerpPoller**: 10s timeout, checked every poll cycle

Stale cancellation is **skipped** during `Entering` and `Recovering` — entry orders
need time to fill on illiquid pairs, and recovery manages its own orders.

### Depth Gate

Entry and rebalance orders are gated by orderbook depth analysis. If the available
depth at the top 3 levels is insufficient for the planned order size, the action is
skipped. Exit/Unwind orders are never depth-gated (closing is mandatory).

---

## Architecture

### Policy Traits

All traits use a **template method** pattern: implementors supply `_inner` methods,
the default wrapper adds structured `tracing` spans and `monotonic_counter` metrics.

| Trait | Purpose | Generic? |
|---|---|---|
| `DecisionPolicy<P: PairState>` | `evaluate(pair) → DeciderAction` | Yes |
| `ExecutionPolicy<P: PairState>` | `execute(action, pair)` — places orders | Yes |
| `MarginPolicy` | `check_account` + `check_position` | No |
| `PriceSource` | `update_snapshot` / `update_ticker` / `get_mid_price` | No |

### PairState Trait

Abstracts any tradable pair type. Both `SpotPerpPair` and `PerpPerpPair` implement it.

| Method | Purpose |
|---|---|
| `total_delta()` | Net delta exposure across all legs |
| `status()` / `set_status()` | Current lifecycle state |
| `apply_event(ZmqEvent) → bool` | Route pair-specific events, return true if evaluation should trigger |
| `refresh_prices(oracle_fn)` | Update leg prices from oracle |
| `refresh_orderbook(bid, ask, bid_depth, ask_depth)` | Update orderbook data for smart order placement |
| `positions_for_margin_check()` | Enumerate positions needing margin validation |
| `is_fully_entered()` / `is_fully_exited()` | Position completeness checks |
| `close_sizes()` | Sizes to fully close both legs |
| `has_sufficient_depth(long, short)` | Orderbook depth gate |
| `recovery_sizes()` | Compute notional-matching sizes for the missing leg |

### Engine Variants

| Strategy | Engine | Event Model |
|---|---|---|
| Spot-Perp | `ArbitrageEngine<SpotPerpPair, D, E, M, P>` | Event-driven via ZMQ subscriber |
| Perp-Perp | `PerpPerpPoller<M, P>` | Poll-based (configurable interval) + event-driven for order updates |

Both engines share the same lifecycle state machine, stale order handling,
margin checks, and command handling.

### StrategyRunner

Top-level dispatch enum that wraps the concrete engine variants:

```rust
enum StrategyRunner {
    SpotPerp(SpotPerpEngine, ZmqEventSubscriber, cmd_rx),
    SpotPerpPaper(SpotPerpPaperEngine, ZmqEventSubscriber, cmd_rx),
    PerpPerp(PerpPerpPoller, long_sub, short_sub, cmd_rx),
}
```

### Paper Trading

`PaperExecution<P, E>` decorates any `ExecutionPolicy`, intercepting all actions:
- Enter/Rebalance/Recover → simulates instant fill, sets `Active`
- Exit/Unwind → simulates close, sets `Inactive`
- No real orders placed. Logs with `mode = "paper"`.

Generic over `P: PairState` — works with any pair type.

### REST Response Handling

All order-related REST calls use `ResponseExt::json_logged()` from `core/src/http/client.rs`.
On parse failure, the full raw body is logged at ERROR level.

| API | Use Case |
|---|---|
| `resp.json_logged(ctx)` | Standard parse + log raw body on failure |
| `check_and_parse(resp, ctx, detector)` | Catches HTTP 200 with error payload |
| `resp.text_logged(ctx)` | Full manual control `(body, status)` |

---

## Event Flow

### ArbitrageEngine (Spot-Perp)

Event-driven — evaluates on every relevant ZMQ event:

```
ZmqEvent arrives
  ├─ OrderBook / Ticker → oracle
  ├─ OrderUpdate → tracker + pair.apply_event + check_tracker_transitions
  └─ FundingRate / PositionUpdate → pair.apply_event
       └─ if true → evaluate_and_execute()
            ├─ margin.check_account (gate)
            ├─ transitional status retry loops
            ├─ refresh prices + orderbook
            ├─ margin.check_position → EMERGENCY EXIT if near liquidation
            └─ decider.evaluate → depth gate → execution.execute
```

### PerpPerpPoller (Perp-Perp)

Poll-based — evaluates on configurable interval:

```
Every poll_interval_secs:
  1. Refresh funding rates (cache or API)
  2. Refresh prices + orderbook from oracle
  3. Sync positions from exchange
  3a. Cancel stale orders (skip during Entering/Recovering)
  3b. Handle transitional statuses (Recovering/Exiting/Unwinding)
  4. Margin checks → EMERGENCY EXIT if near liquidation
  5. Decision → depth gate → execution
```

Events are also processed between polls (OrderBook, Ticker, OrderUpdate, etc.)
for real-time position tracking and lifecycle transitions.

---

## CLI: Strategy Control (`stratctl`)

Control running strategies from the command line. Communicates via ZMQ DEALER→ROUTER
through Redis service discovery.

### Commands

```bash
# List all running strategies with their status
boldtrax-bot stratctl list
boldtrax-bot ctl list          # alias

# Get status of a specific strategy (auto-selects if only one running)
boldtrax-bot stratctl status
boldtrax-bot stratctl status -s "perp_perp_BTCUSDT"

# Pause strategy evaluation (stops new entries/rebalances/exits)
boldtrax-bot stratctl pause
boldtrax-bot stratctl pause -s "perp_perp_BTCUSDT"

# Resume evaluation after pause (e.g. after emergency margin exit)
boldtrax-bot stratctl resume
boldtrax-bot stratctl resume -s "perp_perp_BTCUSDT"

# Graceful exit — chunked unwind over multiple cycles (4 chunks default)
boldtrax-bot stratctl exit

# Force exit — immediate market close of all positions
boldtrax-bot stratctl exit --force
boldtrax-bot stratctl exit -s "perp_perp_BTCUSDT" --force
```

### Status Output

```
Running strategies (1):
  perp_perp_BTCUSDT — Active | pending_orders: 0
```

When paused (e.g. after emergency margin exit):
```
Running strategies (1):
  perp_perp_BTCUSDT — Inactive | pending_orders: 0 [PAUSED]
```

### Strategy Commands

| Command | Effect |
|---|---|
| `GetStatus` | Returns current pair status, pause state, pending order count |
| `Pause` | Sets `paused = true` — evaluation becomes a no-op, events still processed |
| `Resume` | Sets `paused = false` — triggers immediate evaluation |
| `GracefulExit` | Only from `Active`. Starts chunked unwind (4 chunks). Status → `Unwinding` |
| `ForceExit` | From any state with positions. Market close both legs. Status → `Exiting` |

---

## CLI: Manual Trading REPL

Interactive REPL for manual order management via ZMQ.

```bash
boldtrax-bot manual -e binance
boldtrax-bot manual -e bybit
```

### Commands

```
>> help
Available commands:
  balance                           — Account snapshot (balances, margin)
  positions                         — All open positions
  position <pair> <type>            — Single position lookup
  instruments                       — All tradeable instruments
  fundingrate <instrument_key>      — Cached funding rate snapshot
  buy <pair> <type> <size> [price]  — Buy order (market if no price)
  sell <pair> <type> <size> [price] — Sell order (limit if price given)
  cancel <order_id>                 — Cancel an order
  exit                              — Quit REPL
```

### Examples

```bash
>> balance
AccountSnapshot { exchange: Binance, ... }

>> positions
[Position { key: BTCUSDT-BN-SWAP, size: -0.01, ... }]

>> position BTCUSDT swap
Position { key: BTCUSDT-BN-SWAP, size: -0.01, entry_price: 95000, ... }

>> buy BTCUSDT swap 0.01
Order submitted: Order { ... }

>> sell BTCUSDT swap 0.01 95500
Order submitted: Order { ... }

>> cancel abc123
Order canceled: Order { ... }

>> fundingrate BTCUSDT-BN-SWAP
FundingRateSnapshot { funding_rate: 0.0001, mark_price: 95100, ... }
```

The REPL spawns a background task that prints order updates and position changes
in real-time as ZMQ events arrive.

---

## Configuration

### Spot-Perp (`config/strategies/spot_perp.toml`)

```toml
[strategy.spot_perp]
# Minimum perp funding rate to justify entering (0.01% = typical positive rate)
min_funding_threshold = "0.00003"
# Target dollar exposure per side (spot buy + perp short)
target_notional = "280"
# Delta drift threshold to trigger rebalance (% of target_notional)
rebalance_drift_pct = "10"
# Minimum distance from liquidation price as % of mark price
liquidation_buffer_pct = "15"
# Full InstrumentKey strings
spot_instrument = "XRPUSDT-BN-SPOT"
perp_instrument = "XRPUSDT-BN-SWAP"
```

### Perp-Perp (`config/strategies/perp_perp.toml`)

```toml
[strategy.perp_perp]
# Carry direction: "positive" (long leg receives funding) or "negative"
carry_direction = "negative"
# Minimum |long_rate - short_rate| spread to enter a position
min_spread_threshold = "0.0001"
# Optional: exit when spread drops below this value
# If omitted, exits immediately when spread flips sign (< 0)
# exit_threshold = "0.00003"
# Target dollar exposure per side (long + short)
target_notional = "450"
# Delta drift threshold to trigger rebalance (% of target_notional)
rebalance_drift_pct = "10"
# Minimum distance from liquidation price as % of mark price
liquidation_buffer_pct = "10"
# Full InstrumentKey strings — must end in -SWAP
perp_long_instrument = "MIRAUSDT-BN-SWAP"
perp_short_instrument = "MIRAUSDT-BY-SWAP"
# How often to poll funding rates via ZMQ (seconds)
poll_interval_secs = 5
# Cache staleness threshold (seconds)
cache_staleness_secs = 2
```

---

## What NOT to Confuse

- **Spot-Perp**: ONE funding rate (the perp's). Spot has no funding, no position, no liquidation.
- **Perp-Perp**: TWO funding rates. Both legs have positions and liquidation risk.
  These are entirely different data models and decision logic.
- **ArbitrageEngine**: Event-driven, used for spot-perp. Single ZMQ subscriber.
- **PerpPerpPoller**: Poll-based, used for perp-perp. Two ZMQ subscribers (one per exchange).
- **GracefulExit vs ForceExit**: Graceful = chunked unwind (4 rounds), Force = immediate market close.
- **Paused vs Inactive**: Paused blocks evaluation but the engine is still running (events processed).
  Inactive means no positions — ready for new entry when conditions are met.
