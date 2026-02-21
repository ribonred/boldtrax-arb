use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use hmac::{Hmac, Mac};
use reqwest::Request;
use reqwest::header::{AUTHORIZATION, HeaderName};
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};
use url::form_urlencoded;

use crate::http::errors::ClientError;

#[async_trait]
pub trait AuthProvider: Send + Sync {
    async fn apply_auth(&self, request: &mut Request) -> Result<(), ClientError>;
}

#[derive(Debug, Clone)]
pub struct BasicAuth {
    username: String,
    password: Option<String>,
}

impl BasicAuth {
    pub fn new(username: String, password: Option<String>) -> Self {
        Self { username, password }
    }
}

#[async_trait]
impl AuthProvider for BasicAuth {
    async fn apply_auth(&self, request: &mut Request) -> Result<(), ClientError> {
        let auth_value = format!(
            "Basic {}",
            STANDARD.encode(format!(
                "{}:{}",
                self.username,
                self.password.as_deref().unwrap_or("")
            ))
        );
        let header = auth_value.parse().map_err(|_| {
            ClientError::AuthError("Basic Auth header contains invalid characters".to_string())
        })?;
        request.headers_mut().insert(AUTHORIZATION, header);
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct BearerAuth {
    token: String,
}

impl BearerAuth {
    pub fn new(token: String) -> Self {
        Self { token }
    }
}

#[async_trait]
impl AuthProvider for BearerAuth {
    async fn apply_auth(&self, request: &mut Request) -> Result<(), ClientError> {
        let auth_value = format!("Bearer {}", self.token);
        let header = auth_value.parse().map_err(|_| {
            ClientError::AuthError("Bearer token contains invalid header characters".to_string())
        })?;
        request.headers_mut().insert(AUTHORIZATION, header);
        Ok(())
    }
}

/// API Key authentication (custom header)
#[derive(Debug, Clone)]
pub struct ApiKeyAuth {
    header_name: String,
    api_key: String,
}

impl ApiKeyAuth {
    pub fn new(header_name: String, api_key: String) -> Self {
        Self {
            header_name,
            api_key,
        }
    }
}

#[async_trait]
impl AuthProvider for ApiKeyAuth {
    async fn apply_auth(&self, request: &mut Request) -> Result<(), ClientError> {
        let header_name = HeaderName::from_bytes(self.header_name.as_bytes()).map_err(|_| {
            ClientError::AuthError(format!("Invalid API key header name: {}", self.header_name))
        })?;
        let header_value = self.api_key.parse().map_err(|_| {
            ClientError::AuthError("API key value contains invalid header characters".to_string())
        })?;
        request.headers_mut().insert(header_name, header_value);
        Ok(())
    }
}

/// No authentication
#[derive(Debug, Clone)]
pub struct NoAuth;

#[async_trait]
impl AuthProvider for NoAuth {
    async fn apply_auth(&self, _request: &mut Request) -> Result<(), ClientError> {
        Ok(())
    }
}

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
        let url = request.url_mut();
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .map_err(|_| ClientError::AuthError("system clock is before UNIX epoch".to_string()))?;

        // Strip any prior timestamp/signature injected by a previous auth attempt (retry path)
        let mut pairs: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(k, _)| k != "timestamp" && k != "signature" && k != "recvWindow")
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();

        pairs.push(("timestamp".to_string(), now_ms.to_string()));
        pairs.push(("recvWindow".to_string(), self.recv_window_ms.to_string()));

        let mut serializer = form_urlencoded::Serializer::new(String::new());
        for (k, v) in &pairs {
            serializer.append_pair(k, v);
        }
        let query_to_sign = serializer.finish();

        let mut mac = Hmac::<Sha256>::new_from_slice(self.api_secret.as_bytes())
            .map_err(|_| ClientError::AuthError("invalid HMAC key (api_secret)".to_string()))?;
        mac.update(query_to_sign.as_bytes());
        let signature = hex::encode(mac.finalize().into_bytes());

        let final_query = format!("{}&signature={}", query_to_sign, signature);
        url.set_query(Some(&final_query));

        let header_value = self.api_key.parse().map_err(|_| {
            ClientError::AuthError("Binance API key contains invalid header characters".to_string())
        })?;
        request
            .headers_mut()
            .insert(HeaderName::from_static("x-mbx-apikey"), header_value);
        Ok(())
    }
}

// ──────────────────────────────────────────────────────────────────
// WebSocket authentication
// ──────────────────────────────────────────────────────────────────

/// Three-phase WebSocket authentication, mirroring the [`AuthProvider`] pattern
/// for HTTP but adapted to the WebSocket lifecycle:
///
/// 1. **`prepare`** – called *before* the WS connection is opened (e.g. fetch a
///    Binance listen-key via REST).
/// 2. **`handshake_headers`** – returns extra HTTP headers to include in the WS
///    Upgrade request (e.g. `X-Api-Key` for header-authenticated exchanges).
/// 3. **`auth_message`** – called *after* the connection is open; the returned
///    JSON string is sent as the first message (e.g. Bybit's HMAC `auth` frame).
///
/// All methods have default no-op implementations so implementors only override
/// what their exchange needs.
#[async_trait]
pub trait WsAuthProvider: Send + Sync {
    /// Perform any actions required before opening the WebSocket connection.
    async fn prepare(&self) -> Result<(), ClientError> {
        Ok(())
    }

    /// Return additional HTTP headers to include in the WebSocket Upgrade
    /// handshake, or `None` if no extra headers are needed.
    async fn handshake_headers(&self) -> Result<Option<reqwest::header::HeaderMap>, ClientError> {
        Ok(None)
    }

    /// Return a serialised JSON message to send immediately after the connection
    /// is established, or `None` if no post-connect auth message is needed.
    async fn auth_message(&self) -> Result<Option<String>, ClientError> {
        Ok(None)
    }
}

/// Public-stream WebSocket provider – no authentication required.
#[derive(Debug, Clone, Default)]
pub struct NoWsAuth;

#[async_trait]
impl WsAuthProvider for NoWsAuth {}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use reqwest::Client;
    use url::Url;

    /// Verifies that `BinanceHmacAuth` produces a well-formed, valid HMAC-SHA256 signature
    /// by independently recomputing the expected signature from the extracted query params.
    #[tokio::test]
    async fn binance_hmac_produces_valid_signature() {
        let auth = BinanceHmacAuth::new("testApiKey".to_string(), "testSecretKey".to_string());

        let url = Url::parse("https://fapi.binance.com/fapi/v2/balance?orderId=123").unwrap();
        let mut request = Client::new().get(url).build().unwrap();

        auth.apply_auth(&mut request)
            .await
            .expect("apply_auth should not fail");

        let pairs: HashMap<String, String> = request
            .url()
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();

        assert!(pairs.contains_key("timestamp"), "timestamp missing");
        assert!(pairs.contains_key("recvWindow"), "recvWindow missing");
        assert!(pairs.contains_key("signature"), "signature missing");
        assert_eq!(pairs["recvWindow"], "5000");

        // Independently re-derive the expected signature using the extracted timestamp.
        let signed_query = format!(
            "orderId=123&timestamp={}&recvWindow=5000",
            pairs["timestamp"]
        );
        let mut mac = Hmac::<Sha256>::new_from_slice(b"testSecretKey").unwrap();
        mac.update(signed_query.as_bytes());
        let expected = hex::encode(mac.finalize().into_bytes());

        assert_eq!(pairs["signature"], expected, "HMAC signature mismatch");

        // API key header must be set
        let header = request.headers().get("x-mbx-apikey");
        assert!(header.is_some(), "x-mbx-apikey header missing");
        assert_eq!(header.unwrap().to_str().unwrap(), "testApiKey");
    }

    /// Verifies that re-applying auth on a request that already has signing params
    /// (as happens on retry) strips the stale params and replaces them with fresh ones.
    #[tokio::test]
    async fn binance_hmac_strips_stale_signing_params_on_retry() {
        let auth = BinanceHmacAuth::new("key".to_string(), "secret".to_string());

        let url = Url::parse(
            "https://api.binance.com/api/v3/account\
             ?timestamp=1000000&recvWindow=5000&signature=stalesig",
        )
        .unwrap();
        let mut request = Client::new().get(url).build().unwrap();

        auth.apply_auth(&mut request)
            .await
            .expect("apply_auth should not fail");

        let pairs: Vec<(String, String)> = request
            .url()
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();

        let timestamps: Vec<_> = pairs.iter().filter(|(k, _)| k == "timestamp").collect();
        let signatures: Vec<_> = pairs.iter().filter(|(k, _)| k == "signature").collect();

        assert_eq!(
            timestamps.len(),
            1,
            "should have exactly one timestamp after re-auth"
        );
        assert_eq!(
            signatures.len(),
            1,
            "should have exactly one signature after re-auth"
        );
        assert_ne!(
            timestamps[0].1, "1000000",
            "stale timestamp should be replaced"
        );
        assert_ne!(
            signatures[0].1, "stalesig",
            "stale signature should be replaced"
        );
    }
}
