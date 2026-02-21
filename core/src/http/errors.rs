use thiserror::Error;

pub type ClientResult<T> = Result<T, ClientError>;
pub type BuildResult<T> = Result<T, ClientError>;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("invalid url: {0}")]
    InvalidUrl(String),
    #[error("request error: {0}")]
    Request(String),
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("rate limited")]
    RateLimited,
    #[error("timeout")]
    Timeout,
    #[error("server error: {0}")]
    ServerError(u16),
    #[error("invalid response: {0}")]
    InvalidResponse(String),
    #[error("auth error: {0}")]
    AuthError(String),
}

impl From<reqwest::Error> for ClientError {
    fn from(err: reqwest::Error) -> Self {
        if err.is_timeout() {
            return ClientError::Timeout;
        }
        ClientError::Request(err.to_string())
    }
}

impl From<reqwest_middleware::Error> for ClientError {
    fn from(err: reqwest_middleware::Error) -> Self {
        ClientError::Request(err.to_string())
    }
}

impl From<serde_json::Error> for ClientError {
    fn from(err: serde_json::Error) -> Self {
        ClientError::Serialization(err.to_string())
    }
}

impl From<url::ParseError> for ClientError {
    fn from(err: url::ParseError) -> Self {
        ClientError::InvalidUrl(err.to_string())
    }
}
