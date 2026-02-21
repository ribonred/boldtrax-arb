use std::env;
use std::path::PathBuf;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

/// Configuration for logger initialization
pub struct LoggerConfig {
    /// Log level filter (e.g., "debug", "info", "warn", "error")
    pub level: String,
    /// Directory path for log files
    pub log_dir: PathBuf,
    /// Log file prefix
    pub file_prefix: String,
    /// Enable console output
    pub console_enabled: bool,
    /// Enable file output
    pub file_enabled: bool,
}

impl Default for LoggerConfig {
    fn default() -> Self {
        Self {
            level: env::var("LOG_LEVEL").unwrap_or_else(|_| "info".to_string()),
            log_dir: PathBuf::from(env::var("LOG_DIR").unwrap_or_else(|_| "./logs".to_string())),
            file_prefix: env::var("LOG_FILE_PREFIX").unwrap_or_else(|_| "boldtrax".to_string()),
            console_enabled: env::var("LOG_CONSOLE_ENABLED")
                .unwrap_or_else(|_| "true".to_string())
                .parse()
                .unwrap_or(true),
            file_enabled: env::var("LOG_FILE_ENABLED")
                .unwrap_or_else(|_| "true".to_string())
                .parse()
                .unwrap_or(true),
        }
    }
}

/// Initialize the tracing logger with console and file output
///
/// # Environment Variables
/// - `LOG_LEVEL`: Log level filter (default: "info")
/// - `LOG_DIR`: Directory for log files (default: "./logs")
/// - `LOG_FILE_PREFIX`: Prefix for log file names (default: "boldtrax")
/// - `LOG_CONSOLE_ENABLED`: Enable console logging (default: "true")
/// - `LOG_FILE_ENABLED`: Enable file logging (default: "true")
///
/// # Returns
/// Returns a `WorkerGuard` that must be kept alive for the duration of the program.
/// Dropping this guard will flush and close the log file.
///
/// # Example
/// ```no_run
/// use core::init_logger;
///
/// let _guard = init_logger();
/// tracing::info!("Logger initialized");
/// ```
pub fn init_logger() -> WorkerGuard {
    init_logger_with_config(LoggerConfig::default())
}

/// Initialize the tracing logger with custom configuration
///
/// # Arguments
/// * `config` - Logger configuration
///
/// # Returns
/// Returns a `WorkerGuard` that must be kept alive for the duration of the program.
pub fn init_logger_with_config(config: LoggerConfig) -> WorkerGuard {
    // Create log directory if it doesn't exist
    if config.file_enabled {
        std::fs::create_dir_all(&config.log_dir).expect("Failed to create log directory");
    }

    // Set up env filter
    let env_filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(&config.level))
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let mut layers = Vec::new();

    // Console layer
    if config.console_enabled {
        let console_layer = tracing_subscriber::fmt::layer()
            .with_target(true)
            .with_thread_ids(false)
            .with_thread_names(false)
            .with_span_events(FmtSpan::CLOSE)
            .with_ansi(true)
            .boxed();
        layers.push(console_layer);
    }

    // File layer with non-blocking writer
    let (guard, file_layer) = if config.file_enabled {
        let file_appender = tracing_appender::rolling::daily(&config.log_dir, &config.file_prefix);
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

        let layer = tracing_subscriber::fmt::layer()
            .with_writer(non_blocking)
            .with_target(true)
            .with_thread_ids(false)
            .with_thread_names(false)
            .with_span_events(FmtSpan::CLOSE)
            .with_ansi(false)
            .boxed();

        (Some(guard), Some(layer))
    } else {
        (None, None)
    };

    if let Some(layer) = file_layer {
        layers.push(layer);
    }

    tracing_subscriber::registry()
        .with(env_filter)
        .with(layers)
        .init();

    guard.unwrap_or_else(|| {
        let (_, g) = tracing_appender::non_blocking(std::io::sink());
        g
    })
}
