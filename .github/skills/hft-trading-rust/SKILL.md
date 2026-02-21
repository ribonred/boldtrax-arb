---
name: hft-trading-rust
description: Describe what this skill does and when to use it. Include keywords that help agents identify relevant tasks.
---
## Skill: Mock Exchange Implementation

**Purpose**: Create simulated exchange behavior for testing trading logic without real execution

**Context**: When implementing paper trading or testing features, create mock exchanges that:
- Implement all standard exchange traits (MarketData, Account, Trading, Exchange)
- Maintain in-memory state for positions, orders, and balances
- Simulate realistic market behavior including price movements, partial fills, and delays
- Support configuration for different simulation scenarios (high volatility, low liquidity, etc.)

**Key behaviors**:
- Mock funding rates that update periodically based on configuration
- Simulate order execution with configurable fill rates and slippage
- Track positions and calculate unrealized P&L realistically
- Generate WebSocket-style updates for position changes
- Support both successful operations and simulated failures for error testing

**Usage pattern**: MockExchange should be a drop-in replacement for real exchanges, allowing the entire system to run in paper trading mode by swapping exchange implementations.

---

## Skill: Configuration-Driven Risk Management

**Purpose**: Create flexible, testable risk management rules that can be tuned without code changes

**Context**: When implementing risk checks or position management logic:
- Load all thresholds and limits from configuration files
- Make each risk rule a separate, testable component
- Allow risk rules to be enabled/disabled via configuration
- Log all risk decisions with the reasoning and parameter values used

**Configuration structure**:
```toml
[risk]
max_position_size_btc = 10.0
max_leverage = 3.0
delta_rebalance_threshold = 0.1
funding_rate_minimum = 0.0005  # 0.05% per 8h
funding_rate_exit_negative = -0.0001
oi_spike_threshold_pct = 0.20  # 20% change
volume_spike_multiplier = 3.0
max_daily_drawdown_pct = 0.05

[execution]
mode = "paper"  # or "live"
enable_auto_rebalance = true
enable_volatility_reduction = true
```

**Key behaviors**:
- Validate configuration values on startup (return `Result<Config, ConfigError>`)
- Provide sensible defaults for all parameters
- Support per-symbol overrides (e.g., different limits for BTC vs ETH)
- Allow runtime updates for non-critical parameters via API or config reload
- Emit warnings when approaching risk limits before hitting hard stops

**Integration**: Risk manager should accept a configuration struct and expose methods like `check_position_size()`, `should_rebalance()`, `detect_volatility()` that return `Result<Decision, RiskError>` with structured decisions and reasoning.

---

## Skill: Execution Mode Abstraction

**Purpose**: Enable seamless switching between live and paper trading

**Context**: When implementing any trading operation (place order, cancel order, close position):
- Check the execution mode before performing the operation
- Route to MockExchange for paper trading or real exchange for live trading
- Ensure all code paths work identically regardless of mode
- Store paper trading data separately from live data in Redis

**Pattern**:
```rust
enum ExecutionMode {
    Live,
    Paper,
}

// In your exchange factory or registry:
fn create_exchange(name: &str, mode: ExecutionMode, config: &Config) -> Arc<dyn Exchange> {
    match mode {
        ExecutionMode::Live => Arc::new(RealExchange::new(name, config)),
        ExecutionMode::Paper => Arc::new(MockExchange::new(name, config)),
    }
}
```

**Key behaviors**:
- Never allow accidental live execution when in paper mode (return `TradingError::InvalidExecutionMode`)
- Clearly distinguish paper vs live in all logs and metrics
- Use Redis key prefixes to separate data: `paper:position:...` vs `live:position:...`
- Make execution mode visible in monitoring dashboards
- Require explicit confirmation to switch from paper to live mode

---

## Skill: Risk Parameter Validation

**Purpose**: Ensure risk configuration is safe and sensible before starting the system

**Context**: When loading configuration:
- Validate that all required risk parameters are present
- Check that values are within reasonable ranges
- Verify that parameters are internally consistent (e.g., max leverage < liquidation threshold)
- Provide clear error messages for invalid configurations
- Return `Result<Config, ConfigError>` from validation functions

**Validation rules**:
- Position limits must be positive
- Leverage must be >= 1.0 and <= some maximum (e.g., 10.0)
- Percentage thresholds must be between 0.0 and 1.0
- Delta thresholds should be reasonable (e.g., < 0.5 or 50%)
- Funding rate thresholds should be realistic for the market

**Error handling**: Fail fast on startup if configuration is invalid. Don't allow the system to run with dangerous or nonsensical parameters. Use explicit `ConfigError` variants to indicate exactly what's wrong.

---

## Skill: Paper Trading Data Isolation

**Purpose**: Prevent paper trading data from contaminating live data and vice versa

**Context**: When storing or retrieving data from Redis:
- Use distinct key prefixes based on execution mode
- Never mix paper and live positions in the same queries
- Make it obvious in logs which mode is active

**Redis key patterns**:
```
# Live mode
live:position:binance:BTC -> {...}
live:funding:binance:BTC -> {...}
live:pnl:daily -> {...}

# Paper mode
paper:position:binance:BTC -> {...}
paper:funding:binance:BTC -> {...}
paper:pnl:daily -> {...}
```

**Key behaviors**:
- Helper functions that automatically apply the correct prefix based on mode
- Separate Redis databases or keyspaces for paper vs live (optional but recommended)
- Paper trading should not affect any live trading state or trigger live orders
- Clear separation in monitoring and reporting between paper and live results

---

## Skill: Risk Decision Logging

**Purpose**: Make risk management transparent and debuggable

**Context**: When a risk check is performed:
- Log the decision (allowed/rejected)
- Include the specific parameter values used
- Show the actual values that triggered the decision
- Provide context about what action was being evaluated
- Include error details when risk checks fail

**Log format example**:
```
[RISK] Position size check: ALLOWED
  Symbol: BTC, Requested: 5.0, Current: 3.0, Max: 10.0, New Total: 8.0

[RISK] Leverage check: REJECTED - RiskError::LeverageExceeded
  Symbol: BTC, Requested: 5.0x, Max Allowed: 3.0x, Config: risk.max_leverage

[RISK] Rebalance trigger: TRUE
  Current Delta: 0.15, Threshold: 0.1, Config: risk.delta_rebalance_threshold

[RISK] Volatility detected: TRUE
  OI Change: 25%, Threshold: 20%, Volume Spike: 4.2x, Threshold: 3.0x
```

**Key behaviors**:
- Use structured logging (JSON) for easy parsing and analysis
- Include timestamp and execution mode in every log entry
- Make it easy to filter logs by risk decision type
- Log both when rules prevent actions and when they allow them
- Log error type names for explicit error enums

---

## Skill: Critical Error Handling

**Purpose**: Use explicit error types for operations where specific error handling is required

**Context**: When implementing trading operations, risk checks, or configuration validation:
- Define custom error enums using `thiserror`
- Include relevant context in each error variant
- Return `Result<T, SpecificError>` instead of `anyhow::Result<T>`
- Handle each error variant explicitly in calling code

**When to use explicit errors:**
- Order placement/cancellation (insufficient balance, order not found, invalid parameters)
- Position management (size limits, leverage limits, margin requirements)
- Risk validation (threshold violations, limit breaches)
- Configuration loading (missing fields, invalid values, constraint violations)
- State transitions (mode switches, emergency shutdowns)

**Pattern example:**
```rust
// Define errors
#[derive(Error, Debug)]
pub enum TradingError {
    #[error("Insufficient balance: required {required}, available {available}")]
    InsufficientBalance { required: Decimal, available: Decimal },
    
    #[error("Position limit exceeded: {symbol} size {size} > limit {limit}")]
    PositionLimitExceeded { symbol: String, size: Decimal, limit: Decimal },
}

// Use in function signature
async fn place_order(&self, request: OrderRequest) -> Result<Order, TradingError> {
    // Implementation
}

// Handle explicitly
match self.place_order(request).await {
    Ok(order) => { /* success */ },
    Err(TradingError::InsufficientBalance { required, available }) => {
        log::error!("Cannot place order: need {required}, have {available}");
        // Specific handling
    },
    Err(TradingError::PositionLimitExceeded { symbol, size, limit }) => {
        log::warn!("Position size {size} exceeds limit {limit} for {symbol}");
        // Specific handling
    },
}
```

---

## Skill: Retryable Error Handling

**Purpose**: Use anyhow for transient errors that benefit from retry logic

**Context**: When implementing network operations, data fetching, or auxiliary functions:
- Use `anyhow::Result<T>` for the return type
- Add context with `.context()` when propagating errors
- Implement retry logic with exponential backoff for transient failures
- Log retry attempts appropriately

**When to use anyhow:**
- HTTP requests (connection timeouts, network failures)
- WebSocket operations (disconnections, reconnections)
- Market data fetching (transient API errors)
- Cache operations (Redis timeouts, connection issues)
- Metrics collection (non-critical failures)

**Pattern example:**
```rust
// Use anyhow for retryable operations
async fn fetch_ticker(&self, symbol: &str) -> anyhow::Result<Ticker> {
    let response = self.client
        .get(&url)
        .send()
        .await
        .context("Failed to connect to exchange")?;
    
    let data = response
        .json()
        .await
        .context("Failed to parse ticker response")?;
    
    Ok(data)
}

// Implement retry logic
async fn fetch_with_retry(&self, symbol: &str) -> anyhow::Result<Ticker> {
    let mut attempts = 0;
    let max_attempts = 3;
    
    loop {
        match self.fetch_ticker(symbol).await {
            Ok(ticker) => return Ok(ticker),
            Err(e) if attempts < max_attempts => {
                attempts += 1;
                log::warn!("Fetch failed (attempt {}/{}): {}", attempts, max_attempts, e);
                tokio::time::sleep(Duration::from_secs(2_u64.pow(attempts))).await;
            },
            Err(e) => return Err(e).context("All retry attempts failed"),
        }
    }
}
```

---

## Integration Example

When copilot helps you implement a new feature (e.g., position opening logic), it should:

1. **Determine error type**: Is this operation critical (use explicit error enum) or retryable (use anyhow)?
2. **Check execution mode**: Route to appropriate exchange implementation (live vs paper)
3. **Load configuration**: Get relevant risk parameters (return `Result<_, ConfigError>`)
4. **Validate operation**: Check against all configured risk rules (return `Result<_, RiskError>`)
5. **Log risk decision**: Include full context and error types if rejected
6. **Execute trade**: Use mock or real exchange based on mode (return `Result<_, TradingError>`)
7. **Handle errors explicitly**: Match on specific error variants for critical errors
8. **Store results**: Save to correct Redis keyspace (paper vs live)
9. **Emit metrics**: Tag with execution mode and error types

This ensures consistency across all trading operations, provides clear error handling at every level, and makes the system safe to test and tune before deploying with real capital.

---

## Skill: Exchange Trait Implementation Template

**Purpose**: Provide a consistent template when adding new exchange connectors

**Context**: When implementing a new exchange connector:
- Follow the standard structure and patterns
- Implement all required traits
- Use appropriate error types for each operation
- Integrate metrics and logging

**Template structure**:
```rust
// exchanges/[exchange_name]/mod.rs
use crate::core::traits::*;
use crate::core::types::*;
use crate::core::errors::*;

pub struct [Exchange]Exchange {
    client: reqwest::Client,
    api_key: String,
    api_secret: String,
    base_url: String,
    metrics: Arc<MetricsCollector>,
}

impl [Exchange]Exchange {
    pub fn new(
        api_key: String,
        api_secret: String,
        metrics: Arc<MetricsCollector>
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            api_secret,
            base_url: "https://api.[exchange].com".to_string(),
            metrics,
        }
    }
    
    fn sign_request(&self, params: &str) -> String {
        // Exchange-specific signing logic
    }
}

// Implement MarketData trait (uses anyhow for retryable operations)
#[async_trait]
impl MarketData for [Exchange]Exchange {
    async fn get_funding_rate(&self, symbol: &str) -> anyhow::Result<FundingRate> {
        let url = format!("{}/v1/funding_rate", self.base_url);
        
        let response = self.metrics.wrap_request(
            "[exchange]",
            "get_funding_rate",
            async {
                self.client
                    .get(&url)
                    .query(&[("symbol", symbol)])
                    .send()
                    .await
                    .context("Failed to fetch funding rate")?
                    .json::<[Exchange]FundingRate>()
                    .await
                    .context("Failed to parse funding rate")
            }
        ).await?;
        
        // Convert to common type
        Ok(self.convert_funding_rate(response))
    }
    
    // Other MarketData methods...
}

// Implement Trading trait (uses explicit errors for critical operations)
#[async_trait]
impl Trading for [Exchange]Exchange {
    async fn place_order(&self, request: OrderRequest) -> Result<Order, TradingError> {
        // Validate balance first
        let balance = self.get_balance(&request.asset)
            .await
            .map_err(|_| TradingError::InsufficientBalance {
                required: request.size,
                available: Decimal::ZERO,
            })?;
        
        if balance < request.size {
            return Err(TradingError::InsufficientBalance {
                required: request.size,
                available: balance,
            });
        }
        
        // Place order
        let response = self.client
            .post(&format!("{}/v1/order", self.base_url))
            .json(&request)
            .send()
            .await
            .map_err(|_| TradingError::OrderNotFound {
                exchange: "[exchange]".to_string(),
                order_id: "unknown".to_string(),
            })?;
        
        // Parse and convert
        let order_data = response.json::<[Exchange]Order>()
            .await
            .map_err(|_| TradingError::OrderNotFound {
                exchange: "[exchange]".to_string(),
                order_id: "unknown".to_string(),
            })?;
        
        Ok(self.convert_order(order_data))
    }
    
    // Other Trading methods...
}

#[async_trait]
impl Exchange for [Exchange]Exchange {
    fn name(&self) -> &str {
        "[Exchange]"
    }
    
    fn supports_perpetuals(&self) -> bool {
        true  // or false
    }
}
```

**Key implementation points**:
- Use `anyhow::Result` for data fetching methods (MarketData, Account queries)
- Use `Result<T, TradingError>` for trading operations
- Always wrap requests with metrics collector
- Convert exchange-specific types to common types before returning
- Add `.context()` to all error propagations in anyhow functions
- Use explicit error variants in TradingError returns
- Include proper logging at appropriate levels

This template ensures new exchanges follow the established patterns and integrate seamlessly with the rest of the system.