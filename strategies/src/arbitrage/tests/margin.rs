#[cfg(test)]
mod tests {
    use crate::arbitrage::margin::{MarginManager, SpotMarginPolicy};
    use crate::arbitrage::policy::MarginPolicy;
    use boldtrax_core::manager::types::{
        AccountModel, AccountSnapshot, BalanceView, CollateralScope,
    };
    use boldtrax_core::types::{
        Currency, Exchange, InstrumentKey, InstrumentType, Pairs, Position,
    };
    use chrono::Utc;
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    fn create_test_key() -> InstrumentKey {
        InstrumentKey {
            exchange: Exchange::Binance,
            pair: Pairs::BTCUSDT,
            instrument_type: InstrumentType::Swap,
        }
    }

    #[test]
    fn test_margin_manager_check_margin_pass() {
        let manager = MarginManager::new(dec("0.8"), dec("5.0")); // Max leverage 80% locked, 5% liq buffer

        let snapshot = AccountSnapshot {
            exchange: Exchange::Binance,
            model: AccountModel::Unified,
            partitions: vec![],
            balances: vec![BalanceView {
                exchange: Exchange::Binance,
                partition: None,
                asset: Currency::USDT,
                total: dec("1000.0"),
                free: dec("500.0"),
                locked: dec("500.0"), // 50% leverage
                borrowed: dec("0.0"),
                unrealized_pnl: dec("0.0"),
                collateral_scope: CollateralScope::SharedAcrossPartitions,
                as_of_utc: Utc::now(),
            }],
            as_of_utc: Utc::now(),
        };

        assert!(manager.check_account(&snapshot).is_ok());
    }

    #[test]
    fn test_margin_manager_check_margin_fail() {
        let manager = MarginManager::new(dec("0.8"), dec("5.0")); // Max leverage 80% locked

        let snapshot = AccountSnapshot {
            exchange: Exchange::Binance,
            model: AccountModel::Unified,
            partitions: vec![],
            balances: vec![BalanceView {
                exchange: Exchange::Binance,
                partition: None,
                asset: Currency::USDT,
                total: dec("1000.0"),
                free: dec("100.0"),
                locked: dec("900.0"), // 90% leverage, exceeds 80%
                borrowed: dec("0.0"),
                unrealized_pnl: dec("0.0"),
                collateral_scope: CollateralScope::SharedAcrossPartitions,
                as_of_utc: Utc::now(),
            }],
            as_of_utc: Utc::now(),
        };

        assert!(manager.check_account(&snapshot).is_err());
    }

    #[test]
    fn test_margin_manager_check_liquidation_distance_pass() {
        let manager = MarginManager::new(dec("0.8"), dec("5.0")); // 5% liq buffer

        let position = Position {
            key: create_test_key(),
            size: dec("1.0"),
            entry_price: dec("50000.0"),
            unrealized_pnl: dec("0.0"),
            leverage: dec("10.0"),
            liquidation_price: Some(dec("45000.0")), // 10% distance from 50000
        };

        assert!(manager.check_position(&position, dec("50000.0")).is_ok());
    }

    #[test]
    fn test_margin_manager_check_liquidation_distance_fail() {
        let manager = MarginManager::new(dec("0.8"), dec("5.0")); // 5% liq buffer

        let position = Position {
            key: create_test_key(),
            size: dec("1.0"),
            entry_price: dec("50000.0"),
            unrealized_pnl: dec("0.0"),
            leverage: dec("10.0"),
            liquidation_price: Some(dec("48000.0")), // 4% distance from 50000, less than 5%
        };

        assert!(manager.check_position(&position, dec("50000.0")).is_err());
    }

    #[test]
    fn test_margin_manager_check_liquidation_distance_no_liq_price() {
        let manager = MarginManager::new(dec("0.8"), dec("5.0"));

        let position = Position {
            key: create_test_key(),
            size: dec("1.0"),
            entry_price: dec("50000.0"),
            unrealized_pnl: dec("0.0"),
            leverage: dec("1.0"),
            liquidation_price: None, // No liquidation price (e.g., spot or fully collateralized)
        };

        assert!(manager.check_position(&position, dec("50000.0")).is_ok());
    }
    #[test]
    fn test_spot_margin_policy_check_account_always_ok() {
        let policy = SpotMarginPolicy;

        // Even at 100% locked, spot policy doesn't care.
        let snapshot = AccountSnapshot {
            exchange: Exchange::Binance,
            model: AccountModel::Unified,
            partitions: vec![],
            balances: vec![BalanceView {
                exchange: Exchange::Binance,
                partition: None,
                asset: Currency::USDT,
                total: dec("1000.0"),
                free: dec("0.0"),
                locked: dec("1000.0"),
                borrowed: dec("0.0"),
                unrealized_pnl: dec("0.0"),
                collateral_scope: CollateralScope::SharedAcrossPartitions,
                as_of_utc: Utc::now(),
            }],
            as_of_utc: Utc::now(),
        };

        assert!(policy.check_account(&snapshot).is_ok());
    }

    #[test]
    fn test_spot_margin_policy_check_position_always_ok() {
        let policy = SpotMarginPolicy;

        // Even with liquidation price dangerously close, spot policy ignores it.
        let position = Position {
            key: create_test_key(),
            size: dec("1.0"),
            entry_price: dec("50000.0"),
            unrealized_pnl: dec("-5000.0"),
            leverage: dec("50.0"),
            liquidation_price: Some(dec("49999.0")),
        };

        assert!(policy.check_position(&position, dec("50000.0")).is_ok());
    }
}
