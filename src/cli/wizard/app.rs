use inquire::Text;

pub fn prompt_app_config() -> anyhow::Result<String> {
    println!("--- Application Configuration ---");

    let redis_url = Text::new("Redis Connection URL:")
        .with_default("redis://127.0.0.1:6379")
        .prompt()?;

    Ok(redis_url)
}
