use crate::http::auth::AuthProvider;
use crate::http::errors::{BuildResult, ClientError, ClientResult};
use async_trait::async_trait;
use reqwest::{Client, Request, Response, StatusCode};
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware, Middleware, Next};
use reqwest_retry::{RetryTransientMiddleware, policies::ExponentialBackoff};
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, instrument, warn};
use url::Url;

struct AuthMiddleware {
    auth_provider: Arc<dyn AuthProvider>,
}

#[async_trait]
impl Middleware for AuthMiddleware {
    async fn handle(
        &self,
        mut req: Request,
        extensions: &mut http::Extensions,
        next: Next<'_>,
    ) -> reqwest_middleware::Result<Response> {
        self.auth_provider
            .apply_auth(&mut req)
            .await
            .map_err(|e| reqwest_middleware::Error::Middleware(e.into()))?;

        next.run(req, extensions).await
    }
}

pub struct HttpClientBuilder {
    base_url: String,
    timeout: Duration,
    max_retries: u32,
    auth_provider: Option<Arc<dyn AuthProvider>>,
}

impl HttpClientBuilder {
    pub fn new(base_url: String) -> Self {
        Self {
            base_url,
            timeout: Duration::from_secs(30),
            max_retries: 3,
            auth_provider: None,
        }
    }

    pub fn timeout(mut self, secs: u64) -> Self {
        self.timeout = Duration::from_secs(secs);
        self
    }

    pub fn max_retries(mut self, retries: u32) -> Self {
        self.max_retries = retries;
        self
    }

    pub fn auth_provider<T: AuthProvider + 'static>(mut self, provider: T) -> Self {
        self.auth_provider = Some(Arc::new(provider));
        self
    }

    pub fn build(self) -> BuildResult<TracedHttpClient> {
        let base_url = Url::parse(&self.base_url)?;
        let client = Client::builder().timeout(self.timeout).build()?;
        let retry_policy = ExponentialBackoff::builder().build_with_max_retries(self.max_retries);

        // Retry is outermost so that each retry attempt is re-authenticated
        let mut builder = ClientBuilder::new(client)
            .with(RetryTransientMiddleware::new_with_policy(retry_policy));

        if let Some(auth_provider) = self.auth_provider {
            builder = builder.with(AuthMiddleware { auth_provider });
        }

        let client_with_middleware = builder.build();

        Ok(TracedHttpClient {
            client: client_with_middleware,
            base_url: base_url.to_string(),
        })
    }
}

#[derive(Clone)]
pub struct TracedHttpClient {
    client: ClientWithMiddleware,
    base_url: String,
}

impl TracedHttpClient {
    #[instrument(skip(self), fields(method = "GET", url = %path))]
    pub async fn get(&self, path: &str) -> ClientResult<Response> {
        let url = self.build_url(path)?;
        debug!("Sending GET request to {}", url);

        let response = self.client.get(url).send().await?;
        self.handle_response(response).await
    }

    #[instrument(skip(self, body), fields(method = "POST", url = %path))]
    pub async fn post<T: Serialize>(&self, path: &str, body: &T) -> ClientResult<Response> {
        let url = self.build_url(path)?;
        debug!("Sending POST request to {}", url);

        // Serialize body to JSON
        let json_body = serde_json::to_string(body)?;

        let response = self
            .client
            .post(url)
            .header("content-type", "application/json")
            .body(json_body)
            .send()
            .await?;

        self.handle_response(response).await
    }

    #[instrument(skip(self), fields(method = "POST", url = %path))]
    pub async fn post_empty(&self, path: &str) -> ClientResult<Response> {
        let url = self.build_url(path)?;
        debug!("Sending empty POST request to {}", url);

        let response = self
            .client
            .post(url)
            .header("content-type", "application/x-www-form-urlencoded")
            .send()
            .await?;

        self.handle_response(response).await
    }

    /// Perform PUT request with JSON body
    #[instrument(skip(self, body), fields(method = "PUT", url = %path))]
    pub async fn put<T: Serialize>(&self, path: &str, body: &T) -> ClientResult<Response> {
        let url = self.build_url(path)?;
        debug!("Sending PUT request to {}", url);

        // Serialize body to JSON
        let json_body = serde_json::to_string(body)?;

        let response = self
            .client
            .put(url)
            .header("content-type", "application/json")
            .body(json_body)
            .send()
            .await?;

        self.handle_response(response).await
    }

    #[instrument(skip(self), fields(method = "PUT", url = %path))]
    pub async fn put_empty(&self, path: &str) -> ClientResult<Response> {
        let url = self.build_url(path)?;
        debug!("Sending empty PUT request to {}", url);

        let response = self
            .client
            .put(url)
            .header("content-type", "application/x-www-form-urlencoded")
            .send()
            .await?;

        self.handle_response(response).await
    }

    /// Perform DELETE request
    #[instrument(skip(self), fields(method = "DELETE", url = %path))]
    pub async fn delete(&self, path: &str) -> ClientResult<Response> {
        let url = self.build_url(path)?;
        debug!("Sending DELETE request to {}", url);

        let response = self.client.delete(url).send().await?;
        self.handle_response(response).await
    }

    /// Build full URL from path
    fn build_url(&self, path: &str) -> ClientResult<String> {
        let base = Url::parse(&self.base_url)
            .map_err(|e| ClientError::InvalidUrl(format!("invalid base_url: {}", e)))?;
        let url = base
            .join(path)
            .map_err(|e| ClientError::InvalidUrl(format!("invalid path: {}", e)))?;
        Ok(url.to_string())
    }

    /// Handle HTTP response and map errors
    async fn handle_response(&self, response: Response) -> ClientResult<Response> {
        let status = response.status();

        match status {
            StatusCode::OK | StatusCode::CREATED | StatusCode::ACCEPTED => {
                info!("Request successful: {}", status);
                Ok(response)
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                let error_body = Self::extract_error_body(response).await;
                warn!("Unauthorized request: {} - {}", status, error_body);
                Err(ClientError::Unauthorized)
            }
            StatusCode::TOO_MANY_REQUESTS => {
                let error_body = Self::extract_error_body(response).await;
                warn!("Rate limited: {} - {}", status, error_body);
                Err(ClientError::RateLimited)
            }
            StatusCode::REQUEST_TIMEOUT | StatusCode::GATEWAY_TIMEOUT => {
                let error_body = Self::extract_error_body(response).await;
                warn!("Request timeout: {} - {}", status, error_body);
                Err(ClientError::Timeout)
            }
            status if status.is_server_error() => {
                let error_body = Self::extract_error_body(response).await;
                warn!("Server error: {} - {}", status, error_body);
                Err(ClientError::ServerError(status.as_u16()))
            }
            _ => {
                let error_body = Self::extract_error_body(response).await;
                warn!("Request failed with status {}: {}", status, error_body);
                Err(ClientError::InvalidResponse(format!(
                    "HTTP {}: {}",
                    status, error_body
                )))
            }
        }
    }

    async fn extract_error_body(response: Response) -> String {
        let text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unable to read error body".to_string());

        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text)
            && let Ok(pretty) = serde_json::to_string_pretty(&json)
        {
            return pretty;
        }

        text
    }

    /// Get base URL
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::auth::{ApiKeyAuth, BasicAuth, BearerAuth, NoAuth};
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn test_get_request_no_auth() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "ok"})))
            .mount(&mock_server)
            .await;

        let client = HttpClientBuilder::new(mock_server.uri())
            .timeout(5)
            .build()
            .unwrap();

        let response = client.get("/test").await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["status"], "ok");
    }

    #[tokio::test]
    async fn test_post_request_with_basic_auth() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/create"))
            .and(header(
                "authorization",
                "Basic dGVzdF91c2VyOnRlc3RfcGFzcw==",
            ))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "123"})))
            .mount(&mock_server)
            .await;

        let auth = BasicAuth::new("test_user".to_string(), Some("test_pass".to_string()));
        let client = HttpClientBuilder::new(mock_server.uri())
            .auth_provider(auth)
            .build()
            .unwrap();

        let payload = json!({"name": "test"});
        let response = client.post("/api/create", &payload).await.unwrap();

        assert_eq!(response.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn test_bearer_auth() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/secure"))
            .and(header("authorization", "Bearer my_token_123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access": "granted"})))
            .mount(&mock_server)
            .await;

        let auth = BearerAuth::new("my_token_123".to_string());
        let client = HttpClientBuilder::new(mock_server.uri())
            .auth_provider(auth)
            .build()
            .unwrap();

        let response = client.get("/secure").await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_api_key_auth() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/data"))
            .and(header("x-api-key", "secret_key_456"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": "value"})))
            .mount(&mock_server)
            .await;

        let auth = ApiKeyAuth::new("x-api-key".to_string(), "secret_key_456".to_string());
        let client = HttpClientBuilder::new(mock_server.uri())
            .auth_provider(auth)
            .build()
            .unwrap();

        let response = client.get("/api/data").await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_unauthorized_error() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/protected"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&mock_server)
            .await;

        let client = HttpClientBuilder::new(mock_server.uri()).build().unwrap();

        let result = client.get("/protected").await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ClientError::Unauthorized));
    }

    #[tokio::test]
    async fn test_rate_limit_error() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/limited"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&mock_server)
            .await;

        let client = HttpClientBuilder::new(mock_server.uri())
            .max_retries(0) // Disable retries for this test
            .build()
            .unwrap();

        let result = client.get("/api/limited").await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ClientError::RateLimited));
    }

    #[tokio::test]
    async fn test_server_error() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/error"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock_server)
            .await;

        let client = HttpClientBuilder::new(mock_server.uri())
            .max_retries(0)
            .build()
            .unwrap();

        let result = client.get("/api/error").await;
        assert!(result.is_err());
        match result.unwrap_err() {
            ClientError::ServerError(code) => assert_eq!(code, 500),
            _ => panic!("Expected ServerError"),
        }
    }

    #[tokio::test]
    async fn test_post_with_json_body() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/users"))
            .respond_with(
                ResponseTemplate::new(201).set_body_json(json!({"id": 42, "name": "John"})),
            )
            .mount(&mock_server)
            .await;

        let client = HttpClientBuilder::new(mock_server.uri()).build().unwrap();

        let payload = json!({"name": "John", "email": "john@example.com"});
        let response = client.post("/api/users", &payload).await.unwrap();

        assert_eq!(response.status(), StatusCode::CREATED);
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["id"], 42);
    }

    #[tokio::test]
    async fn test_no_auth_provider() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/public"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"public": true})))
            .mount(&mock_server)
            .await;

        let client = HttpClientBuilder::new(mock_server.uri())
            .auth_provider(NoAuth)
            .build()
            .unwrap();

        let response = client.get("/public").await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
