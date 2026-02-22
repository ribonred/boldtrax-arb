use inquire::{CustomType, Text};

pub fn prompt_app_config() -> anyhow::Result<(String, u64)> {
    println!("--- Application Configuration ---");

    let redis_url = Text::new("Redis Connection URL:")
        .with_default("redis://127.0.0.1:6379")
        .prompt()?;

    let update_interval_ms = CustomType::<u64>::new("Main loop polling frequency (ms):")
        .with_default(1000)
        .prompt()?;

    Ok((redis_url, update_interval_ms))
}
