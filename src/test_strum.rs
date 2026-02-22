use strum_macros::{EnumProperty, EnumString, Display};
use strum::EnumProperty as _;

#[derive(Debug, EnumString, Display, EnumProperty)]
#[strum(serialize_all = "lowercase")]
pub enum Exchange {
    #[strum(props(short_code="BN"))]
    Binance,
    #[strum(props(short_code="BY"))]
    Bybit,
}

fn main() {
    let e = Exchange::Binance;
    println!("{} {}", e, e.get_str("short_code").unwrap());
}
