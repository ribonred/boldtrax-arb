// Logger initialisation lives in the binary (`main.rs`) via `tracing-config`.
// See `tracing.toml` in the project root to control filters, formatters, and
// output destinations. Library crates only emit events — they never init the
// global subscriber.
