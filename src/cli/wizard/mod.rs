pub mod app;
pub mod exchanges;
pub mod risk;
pub mod strategies;

use boldtrax_core::config::types::AppConfig;
use boldtrax_core::types::ExecutionMode;
use inquire::{Confirm, MultiSelect, Select};
use std::fs;
use std::path::Path;

pub fn run_wizard() -> anyhow::Result<()> {
    println!("Welcome to the Boldtrax Configuration Wizard!");

    ensure_config_dirs()?;

    let options = vec![
        "Full Setup (Generates local.toml)",
        "Configure App Settings",
        "Configure Risk Parameters",
        "Configure Exchange",
        "Configure Strategy",
        "Exit",
    ];

    loop {
        let choice = Select::new("What would you like to configure?", options.clone()).prompt()?;

        match choice {
            "Full Setup (Generates local.toml)" => {
                full_setup()?;
                break;
            }
            "Configure App Settings" => {
                configure_app()?;
            }
            "Configure Risk Parameters" => {
                configure_risk()?;
            }
            "Configure Exchange" => {
                configure_exchange()?;
            }
            "Configure Strategy" => {
                configure_strategy()?;
            }
            "Exit" => break,
            _ => unreachable!(),
        }
    }

    Ok(())
}

fn ensure_config_dirs() -> anyhow::Result<()> {
    fs::create_dir_all("config/exchanges")?;
    fs::create_dir_all("config/strategies")?;
    Ok(())
}

fn full_setup() -> anyhow::Result<()> {
    let mode_options = vec!["Paper Trading", "Live Trading"];
    let mode_choice = Select::new("Select Execution Mode:", mode_options).prompt()?;
    let execution_mode = match mode_choice {
        "Live Trading" => ExecutionMode::Live,
        _ => ExecutionMode::Paper,
    };

    let redis_url = app::prompt_app_config()?;
    let risk = risk::prompt_risk_config()?;

    let exchanges = std::collections::HashMap::new();
    if Confirm::new("Would you like to configure exchanges now?")
        .with_default(true)
        .prompt()?
    {
        let configs = prompt_exchange_config()?;
        for (name, config) in configs {
            save_exchange_config(&name, config)?;
        }
    }

    if Confirm::new("Would you like to configure a strategy now?")
        .with_default(true)
        .prompt()?
    {
        configure_strategy()?;
    }

    let config = AppConfig {
        execution_mode,
        redis_url,
        risk,
        exchanges,
        ..Default::default()
    };

    let toml_string = toml::to_string_pretty(&config)?;
    fs::write("config/local.toml", toml_string)?;

    println!("Configuration saved to config/local.toml successfully!");
    Ok(())
}

fn configure_app() -> anyhow::Result<()> {
    let redis_url = app::prompt_app_config()?;

    let mut config = if Path::new("config/local.toml").exists() {
        let content = fs::read_to_string("config/local.toml")?;
        toml::from_str::<AppConfig>(&content).unwrap_or_default()
    } else {
        AppConfig::default()
    };

    config.redis_url = redis_url;

    let toml_string = toml::to_string_pretty(&config)?;
    fs::write("config/local.toml", toml_string)?;

    println!("App configuration updated in config/local.toml!");
    Ok(())
}

fn configure_risk() -> anyhow::Result<()> {
    let risk = risk::prompt_risk_config()?;

    let mut config = if Path::new("config/local.toml").exists() {
        let content = fs::read_to_string("config/local.toml")?;
        toml::from_str::<AppConfig>(&content).unwrap_or_default()
    } else {
        AppConfig::default()
    };

    config.risk = risk;

    let toml_string = toml::to_string_pretty(&config)?;
    fs::write("config/local.toml", toml_string)?;

    println!("Risk configuration updated in config/local.toml!");
    Ok(())
}

fn configure_exchange() -> anyhow::Result<()> {
    let configs = prompt_exchange_config()?;
    for (name, config) in configs {
        save_exchange_config(&name, config)?;
    }
    Ok(())
}

fn save_exchange_config(name: &str, config: toml::Value) -> anyhow::Result<()> {
    #[derive(serde::Serialize)]
    struct ExchangeOverride {
        exchanges: std::collections::HashMap<String, toml::Value>,
    }

    let mut exchanges_map = std::collections::HashMap::new();
    exchanges_map.insert(name.to_string(), config);

    let override_config = ExchangeOverride {
        exchanges: exchanges_map,
    };

    let toml_string = toml::to_string_pretty(&override_config)?;
    let path = format!("config/exchanges/{}.toml", name);
    fs::write(&path, toml_string)?;

    println!("Exchange configuration saved to {}!", path);
    Ok(())
}

fn prompt_exchange_config() -> anyhow::Result<Vec<(String, toml::Value)>> {
    let wizards = exchanges::get_available_wizards();
    let options: Vec<String> = wizards.iter().map(|w| w.exchange().to_string()).collect();

    let choices = MultiSelect::new(
        "Select Exchanges to configure (Space to select, Enter to confirm):",
        options,
    )
    .prompt()?;

    let mut configs = Vec::new();
    for choice in choices {
        let wizard = wizards
            .iter()
            .find(|w| w.exchange().to_string() == choice)
            .unwrap();

        println!("\n--- Configuring {} ---", choice);
        let config = wizard.prompt()?;
        configs.push((choice, config));
    }

    Ok(configs)
}

fn configure_strategy() -> anyhow::Result<()> {
    let wizards = strategies::get_available_strategy_wizards();
    let options: Vec<String> = wizards.iter().map(|w| w.kind().to_string()).collect();

    let choice = Select::new("Select strategy to configure:", options).prompt()?;

    let wizard = wizards
        .iter()
        .find(|w| w.kind().to_string() == choice)
        .unwrap();

    let config = wizard.prompt()?;
    save_strategy_config(&choice, config)?;
    Ok(())
}

fn save_strategy_config(kind: &str, config: toml::Value) -> anyhow::Result<()> {
    #[derive(serde::Serialize)]
    struct StrategyOverride {
        strategy: std::collections::HashMap<String, toml::Value>,
    }

    // Normalise to snake_case file name (SpotPerp -> spot_perp)
    let file_name = kind
        .chars()
        .enumerate()
        .fold(String::new(), |mut acc, (i, c)| {
            if c.is_uppercase() && i > 0 {
                acc.push('_');
            }
            acc.push(c.to_ascii_lowercase());
            acc
        });

    let mut strategy_map = std::collections::HashMap::new();
    strategy_map.insert(file_name.clone(), config);

    let override_config = StrategyOverride {
        strategy: strategy_map,
    };

    let toml_string = toml::to_string_pretty(&override_config)?;
    let path = format!("config/strategies/{}.toml", file_name);
    fs::write(&path, &toml_string)?;

    println!("\nStrategy configuration saved to {}!", path);
    Ok(())
}
