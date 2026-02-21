use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

use crate::types::{Currency, Exchange};

#[derive(
    rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash,
)]
#[archive(check_bytes)]
#[repr(u8)]
pub enum AccountModel {
    Unified,
    Segmented,
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[archive(check_bytes)]
#[repr(u8)]
pub enum PartitionKind {
    Unified,
    Spot,
    Swap,
    Margin,
    Funding,
    Options,
    Other(String),
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[archive(check_bytes)]
pub struct AccountPartitionRef {
    pub exchange: Exchange,
    pub kind: PartitionKind,
    pub account_id: Option<String>,
}

#[derive(
    rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash,
)]
#[archive(check_bytes)]
#[repr(u8)]
pub enum CollateralScope {
    SharedAcrossPartitions,
    PartitionOnly,
}

#[derive(
    rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash,
)]
#[archive(check_bytes)]
#[repr(u8)]
pub enum MarginMode {
    Cross,
    Isolated,
    Portfolio,
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct BalanceQuery {
    pub exchange: Exchange,
    pub partition: Option<AccountPartitionRef>,
    pub asset: Option<Currency>,
    pub include_zero_balances: bool,
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct BalanceView {
    pub exchange: Exchange,
    pub partition: Option<AccountPartitionRef>,
    pub asset: Currency,
    pub total: Decimal,
    pub free: Decimal,
    pub locked: Decimal,
    pub borrowed: Decimal,
    pub unrealized_pnl: Decimal,
    pub collateral_scope: CollateralScope,
    pub as_of_utc: DateTime<Utc>,
}

impl BalanceView {
    pub fn net_equity(&self) -> Decimal {
        self.total + self.unrealized_pnl - self.borrowed
    }
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct CollateralSummary {
    pub exchange: Exchange,
    pub model: AccountModel,
    pub quote_asset: Currency,
    pub partition: Option<AccountPartitionRef>,
    pub effective_collateral: Decimal,
    pub total_balance: Decimal,
    pub total_borrowed: Decimal,
    pub total_unrealized_pnl: Decimal,
    pub as_of_utc: DateTime<Utc>,
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct MarginStateView {
    pub exchange: Exchange,
    pub partition: Option<AccountPartitionRef>,
    pub mode: MarginMode,
    pub margin_ratio: Option<Decimal>,
    pub initial_margin_ratio: Option<Decimal>,
    pub maintenance_margin_ratio: Option<Decimal>,
    pub as_of_utc: DateTime<Utc>,
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq)]
#[archive(check_bytes)]
pub struct AccountSnapshot {
    pub exchange: Exchange,
    pub model: AccountModel,
    pub partitions: Vec<AccountPartitionRef>,
    pub balances: Vec<BalanceView>,
    pub as_of_utc: DateTime<Utc>,
}

impl AccountSnapshot {
    pub fn balances_for_partition(
        &self,
        partition: Option<&AccountPartitionRef>,
    ) -> Vec<&BalanceView> {
        self.balances
            .iter()
            .filter(|balance| match (&balance.partition, partition) {
                (None, None) => true,
                (Some(b), Some(p)) => b == p,
                _ => false,
            })
            .collect()
    }

    pub fn effective_collateral(
        &self,
        quote_asset: Currency,
        partition: Option<&AccountPartitionRef>,
    ) -> Decimal {
        self.balances
            .iter()
            .filter(|balance| balance.asset == quote_asset)
            .filter(|balance| match self.model {
                // Unified + no partition: all balances count toward total effective collateral
                AccountModel::Unified => match partition {
                    None => true,
                    Some(p) => match balance.collateral_scope {
                        CollateralScope::SharedAcrossPartitions => true,
                        CollateralScope::PartitionOnly => balance.partition.as_ref() == Some(p),
                    },
                },
                AccountModel::Segmented => match (&balance.partition, partition) {
                    (Some(b), Some(p)) => b == p,
                    _ => false,
                },
            })
            .map(|balance| balance.net_equity())
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn partition(exchange: Exchange, kind: PartitionKind, account_id: &str) -> AccountPartitionRef {
        AccountPartitionRef {
            exchange,
            kind,
            account_id: Some(account_id.to_string()),
        }
    }

    #[test]
    fn balance_net_equity_is_total_plus_pnl_minus_borrowed() {
        let balance = BalanceView {
            exchange: Exchange::Bybit,
            partition: None,
            asset: Currency::USDT,
            total: dec!(1200),
            free: dec!(1100),
            locked: dec!(100),
            borrowed: dec!(200),
            unrealized_pnl: dec!(50),
            collateral_scope: CollateralScope::SharedAcrossPartitions,
            as_of_utc: Utc::now(),
        };

        assert_eq!(balance.net_equity(), dec!(1050));
    }

    #[test]
    fn unified_effective_collateral_uses_shared_and_matching_partition_only() {
        let swap_partition = partition(Exchange::Bybit, PartitionKind::Swap, "swap-1");
        let spot_partition = partition(Exchange::Bybit, PartitionKind::Spot, "spot-1");

        let snapshot = AccountSnapshot {
            exchange: Exchange::Bybit,
            model: AccountModel::Unified,
            partitions: vec![swap_partition.clone(), spot_partition.clone()],
            balances: vec![
                BalanceView {
                    exchange: Exchange::Bybit,
                    partition: None,
                    asset: Currency::USDT,
                    total: dec!(1000),
                    free: dec!(1000),
                    locked: dec!(0),
                    borrowed: dec!(0),
                    unrealized_pnl: dec!(0),
                    collateral_scope: CollateralScope::SharedAcrossPartitions,
                    as_of_utc: Utc::now(),
                },
                BalanceView {
                    exchange: Exchange::Bybit,
                    partition: Some(swap_partition.clone()),
                    asset: Currency::USDT,
                    total: dec!(200),
                    free: dec!(200),
                    locked: dec!(0),
                    borrowed: dec!(0),
                    unrealized_pnl: dec!(0),
                    collateral_scope: CollateralScope::PartitionOnly,
                    as_of_utc: Utc::now(),
                },
                BalanceView {
                    exchange: Exchange::Bybit,
                    partition: Some(spot_partition),
                    asset: Currency::USDT,
                    total: dec!(300),
                    free: dec!(300),
                    locked: dec!(0),
                    borrowed: dec!(0),
                    unrealized_pnl: dec!(0),
                    collateral_scope: CollateralScope::PartitionOnly,
                    as_of_utc: Utc::now(),
                },
            ],
            as_of_utc: Utc::now(),
        };

        assert_eq!(
            snapshot.effective_collateral(Currency::USDT, Some(&swap_partition)),
            dec!(1200)
        );
    }

    #[test]
    fn segmented_effective_collateral_requires_partition_match() {
        let futures_partition = partition(Exchange::Binance, PartitionKind::Swap, "futures");

        let snapshot = AccountSnapshot {
            exchange: Exchange::Binance,
            model: AccountModel::Segmented,
            partitions: vec![futures_partition.clone()],
            balances: vec![BalanceView {
                exchange: Exchange::Binance,
                partition: Some(futures_partition.clone()),
                asset: Currency::USDT,
                total: dec!(500),
                free: dec!(500),
                locked: dec!(0),
                borrowed: dec!(0),
                unrealized_pnl: dec!(25),
                collateral_scope: CollateralScope::PartitionOnly,
                as_of_utc: Utc::now(),
            }],
            as_of_utc: Utc::now(),
        };

        assert_eq!(
            snapshot.effective_collateral(Currency::USDT, Some(&futures_partition)),
            dec!(525)
        );
        assert_eq!(snapshot.effective_collateral(Currency::USDT, None), dec!(0));
    }
}
