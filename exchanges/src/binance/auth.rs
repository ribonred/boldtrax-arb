use async_trait::async_trait;
use boldtrax_core::http::auth::{AuthProvider, apply_binance_hmac_auth};
use boldtrax_core::http::errors::ClientError;
use boldtrax_core::http::reqwest::Request;

#[derive(Debug, Clone)]
pub struct BinanceHmacAuth {
    api_key: String,
    api_secret: String,
    recv_window_ms: u64,
}

impl BinanceHmacAuth {
    pub fn new(api_key: String, api_secret: String) -> Self {
        Self {
            api_key,
            api_secret,
            recv_window_ms: 5_000,
        }
    }

    pub fn with_recv_window_ms(mut self, recv_window_ms: u64) -> Self {
        self.recv_window_ms = recv_window_ms;
        self
    }
}

#[async_trait]
impl AuthProvider for BinanceHmacAuth {
    async fn apply_auth(&self, request: &mut Request) -> Result<(), ClientError> {
        apply_binance_hmac_auth(
            request,
            &self.api_key,
            &self.api_secret,
            self.recv_window_ms,
            &[],
            &[],
        )
    }
}
