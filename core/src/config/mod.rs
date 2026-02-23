pub mod errors;
pub mod types;

use figment::{
    Figment,
    providers::{Env, Format, Toml},
};

use errors::ConfigError;
use types::AppConfig;

impl AppConfig {
    pub fn load() -> Result<Self, ConfigError> {
        let mut figment = Figment::new()
            // 1. Base defaults (committed to git)
            .merge(Toml::file("config/default.toml"))
            // 2. User overrides (git-ignored)
            .merge(Toml::file("config/local.toml"));

        // 3. Exchange-specific configs (git-ignored)
        if let Ok(entries) = std::fs::read_dir("config/exchanges") {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("toml") {
                    figment = figment.merge(Toml::file(path));
                }
            }
        }

        // 4. Strategy-specific configs (git-ignored)
        if let Ok(entries) = std::fs::read_dir("config/strategies") {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("toml") {
                    figment = figment.merge(Toml::file(path));
                }
            }
        }

        // 5. Environment Variables (highest precedence)
        figment = figment.merge(Env::prefixed("BOLDTRAX_").split("__"));

        let config: AppConfig = figment.extract()?;

        let validation_errors = config.validate();
        if !validation_errors.is_empty() {
            return Err(ConfigError::ValidationFailed {
                reason: validation_errors.join(", "),
            });
        }

        Ok(config)
    }
}
