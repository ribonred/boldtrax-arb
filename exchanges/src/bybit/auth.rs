//! Bybit V5 HMAC-SHA256 authentication provider.
//!
//! Delegates to [`apply_bybit_hmac_auth`] in core, following the same pattern
//! as [`AsterHmacAuth`].

use async_trait::async_trait;
use boldtrax_core::http::auth::{AuthProvider, apply_bybit_hmac_auth};
use boldtrax_core::http::errors::ClientError;
use boldtrax_core::http::reqwest::Request;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct BybitHmacAuth {
    api_key: String,
    api_secret: String,
    recv_window_ms: u64,
}

impl BybitHmacAuth {
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
impl AuthProvider for BybitHmacAuth {
    async fn apply_auth(&self, request: &mut Request) -> Result<(), ClientError> {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .map_err(|_| ClientError::AuthError("system clock is before UNIX epoch".to_string()))?;

        apply_bybit_hmac_auth(
            request,
            &self.api_key,
            &self.api_secret,
            self.recv_window_ms,
            now_ms,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boldtrax_core::http::auth::hmac_sha256_hex;
    use boldtrax_core::http::reqwest::{Client, Url};

    #[tokio::test]
    async fn bybit_hmac_produces_valid_signature() {
        let url =
            Url::parse("https://api.bybit.com/v5/market/tickers?category=linear&symbol=BTCUSDT")
                .unwrap();
        let mut request = Client::new().get(url).build().unwrap();

        let auth = BybitHmacAuth::new("testApiKey".to_string(), "testSecretKey".to_string());
        auth.apply_auth(&mut request)
            .await
            .expect("apply_auth should not fail");

        let headers = request.headers();
        assert!(headers.get("x-bapi-api-key").is_some(), "x-bapi-api-key missing");
        assert!(headers.get("x-bapi-timestamp").is_some(), "x-bapi-timestamp missing");
        assert!(headers.get("x-bapi-recv-window").is_some(), "x-bapi-recv-window missing");
        assert!(headers.get("x-bapi-sign").is_some(), "x-bapi-sign missing");

        assert_eq!(
            headers.get("x-bapi-api-key").unwrap().to_str().unwrap(),
            "testApiKey"
        );
        assert_eq!(
            headers.get("x-bapi-recv-window").unwrap().to_str().unwrap(),
            "5000"
        );

        // Verify the signature by re-computing it
        let timestamp = headers
            .get("x-bapi-timestamp")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let payload = "category=linear&symbol=BTCUSDT";
        let sign_str = format!("{}testApiKey5000{}", timestamp, payload);
        let expected = hmac_sha256_hex(b"testSecretKey", sign_str.as_bytes());

        assert_eq!(
            headers.get("x-bapi-sign").unwrap().to_str().unwrap(),
            expected,
            "HMAC signature mismatch"
        );
    }

    #[tokio::test]
    async fn bybit_hmac_post_with_body() {
        let url = Url::parse("https://api.bybit.com/v5/order/create").unwrap();
        let body = r#"{"category":"linear","symbol":"BTCUSDT","side":"Buy","orderType":"Limit"}"#;
        let mut request = Client::new()
            .post(url)
            .body(body.to_string())
            .build()
            .unwrap();

        let auth = BybitHmacAuth::new("testApiKey".to_string(), "testSecretKey".to_string());
        auth.apply_auth(&mut request)
            .await
            .expect("apply_auth should not fail");

        let headers = request.headers();
        let timestamp = headers
            .get("x-bapi-timestamp")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        // For POST, payload = raw body
        let sign_str = format!("{}testApiKey5000{}", timestamp, body);
        let expected = hmac_sha256_hex(b"testSecretKey", sign_str.as_bytes());

        assert_eq!(
            headers.get("x-bapi-sign").unwrap().to_str().unwrap(),
            expected,
            "HMAC signature mismatch for POST body"
        );
    }

    #[tokio::test]
    async fn bybit_hmac_custom_recv_window() {
        let url = Url::parse("https://api.bybit.com/v5/position/list?category=linear").unwrap();
        let mut request = Client::new().get(url).build().unwrap();

        let auth = BybitHmacAuth::new("key".to_string(), "secret".to_string())
            .with_recv_window_ms(10_000);
        auth.apply_auth(&mut request).await.unwrap();

        assert_eq!(
            request
                .headers()
                .get("x-bapi-recv-window")
                .unwrap()
                .to_str()
                .unwrap(),
            "10000"
        );
    }
}
