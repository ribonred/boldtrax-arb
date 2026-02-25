use async_trait::async_trait;
use boldtrax_core::http::auth::{AuthProvider, apply_binance_hmac_auth};
use boldtrax_core::http::errors::ClientError;
use boldtrax_core::http::reqwest::Request;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct AsterHmacAuth {
    api_key: String,
    api_secret: String,
    recv_window_ms: u64,
}

impl AsterHmacAuth {
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
impl AuthProvider for AsterHmacAuth {
    async fn apply_auth(&self, request: &mut Request) -> Result<(), ClientError> {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .map_err(|_| ClientError::AuthError("system clock is before UNIX epoch".to_string()))?;

        let now_str = now_ms.to_string();

        apply_binance_hmac_auth(
            request,
            &self.api_key,
            &self.api_secret,
            self.recv_window_ms,
            &[
                ("nonce", &now_str),
                ("user", &self.api_key),
                ("signer", &self.api_key),
            ],
            &["nonce", "user", "signer"],
        )
    }
}
