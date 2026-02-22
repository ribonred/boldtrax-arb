use strum_macros::{EnumProperty, EnumString, Display};
use strum::EnumProperty as _;
use std::str::FromStr;

#[derive(Debug, EnumString, Display, EnumProperty, PartialEq)]
pub enum Exchange {
    #[strum(to_string = "binance", serialize = "BN", props(short_code="BN"))]
    Binance,
    #[strum(to_string = "bybit", serialize = "BY", props(short_code="BY"))]
    Bybit,
}

fn main() {
    let e = Exchange::Binance;
    println!("Display: {}", e);
    println!("Prop: {}", e.get_str("short_code").unwrap());
    println!("Parse BN: {:?}", Exchange::from_str("BN").unwrap());
    println!("Parse binance: {:?}", Exchange::from_str("binance").unwrap());
}
