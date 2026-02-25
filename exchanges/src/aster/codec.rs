//! `client_order_id` encoding/decoding for Aster.
//!
//! Aster mirrors the Binance Futures API, so the same constraints apply:
//! - Max 36 characters
//! - Alphanumeric + hyphens only
//!
//! The `internal_id` is encoded directly as the `clientOrderId`.

const MAX_LEN: usize = 36;

pub struct AsterClientOrderIdCodec;

impl AsterClientOrderIdCodec {
    pub fn encode(&self, internal_id: &str, _strategy_id: &str) -> String {
        if internal_id.len() <= MAX_LEN {
            internal_id.to_string()
        } else {
            internal_id[..MAX_LEN].to_string()
        }
    }

    pub fn decode_strategy_id(&self, client_order_id: &str) -> Option<String> {
        let segment = client_order_id.split('-').next()?;
        if segment.is_empty() {
            return None;
        }
        Some(segment.to_string())
    }

    pub fn decode_internal_id(&self, client_order_id: &str) -> String {
        client_order_id.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codec() -> AsterClientOrderIdCodec {
        AsterClientOrderIdCodec
    }

    #[test]
    fn round_trip_standard() {
        let c = codec();
        let internal_id = "funding_arb-1737410400000-AS-0";
        let encoded = c.encode(internal_id, "funding_arb");
        assert_eq!(encoded, internal_id);
        assert_eq!(c.decode_strategy_id(&encoded), Some("funding_arb".into()));
        assert_eq!(c.decode_internal_id(&encoded), internal_id);
    }

    #[test]
    fn truncated_still_decodes_strategy() {
        let c = codec();
        let long_id = "very_long_strategy_name-1737410400000-AS-0";
        assert!(long_id.len() > 36);
        let encoded = c.encode(long_id, "very_long_strategy_name");
        assert_eq!(encoded.len(), 36);
        assert_eq!(
            c.decode_strategy_id(&encoded),
            Some("very_long_strategy_name".into())
        );
    }

    #[test]
    fn encoded_length_within_limit() {
        let c = codec();
        let id = "a".repeat(100);
        assert!(c.encode(&id, "").len() <= MAX_LEN);
    }
}
