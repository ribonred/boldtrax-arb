use thiserror::Error;

#[derive(Debug, Clone, Error)]
pub enum ConfigError {
    #[error("Missing required configuration: {field}")]
    MissingField { field: String },

    #[error("Invalid value for {field}: {value} (expected {expected})")]
    InvalidValue {
        field: String,
        value: String,
        expected: String,
    },

    #[error("Configuration validation failed: {reason}")]
    ValidationFailed { reason: String },

    #[error("Failed to load configuration: {0}")]
    LoadError(String),
}

impl From<figment::Error> for ConfigError {
    fn from(err: figment::Error) -> Self {
        ConfigError::LoadError(err.to_string())
    }
}
