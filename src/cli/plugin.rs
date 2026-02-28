use crate::config::Config;
use crate::plugins::registry;

/// List all known plugins with installed/enabled status
pub async fn list() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load().ok();

    let known = registry::known_plugins();
    if known.is_empty() {
        println!("No plugins available yet.");
        return Ok(());
    }

    println!("Available plugins:\n");
    for entry in &known {
        let status = match &config {
            Some(cfg) if cfg.plugins.contains_key(&entry.name) => "installed",
            _ => "not installed",
        };

        println!(
            "  {} v{} — {} [{}]",
            entry.name, entry.version, entry.description, status
        );
    }

    if let Some(cfg) = &config {
        // Check for unknown plugins in config
        for name in cfg.plugins.keys() {
            if registry::find_plugin(name).is_none() {
                println!("\n  Warning: unknown plugin '{}' in config", name);
            }
        }
    }

    Ok(())
}

/// Add a plugin to the entity config
pub async fn add(name: String) -> Result<(), Box<dyn std::error::Error>> {
    let entry = registry::find_plugin(&name).ok_or_else(|| {
        format!(
            "Unknown plugin: '{}'. Run `echo-system plugin list` to see available plugins.",
            name
        )
    })?;

    if !entry.available {
        println!(
            "Plugin '{}' is registered but not yet implemented.",
            entry.name
        );
        println!("It will be available in a future release.");
        return Ok(());
    }

    let config = Config::load()?;

    if config.plugins.contains_key(&name) {
        println!("Plugin '{}' is already installed.", name);
        return Ok(());
    }

    // Read the current config file and append the plugin section
    let root_dir = config.root_dir()?;
    let config_path = root_dir.join("echo-system.toml");
    let mut content = std::fs::read_to_string(&config_path)?;

    content.push_str(&format!("\n[plugins.{}]\n", name));

    // Create plugin directory
    let plugin_dir = root_dir.join("plugins").join(&name);
    std::fs::create_dir_all(&plugin_dir)?;

    std::fs::write(&config_path, content)?;

    println!("Plugin '{}' installed.", name);
    println!("Configure it in echo-system.toml under [plugins.{}]", name);

    Ok(())
}

/// Remove a plugin from the entity config
pub async fn remove(name: String) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load()?;

    if !config.plugins.contains_key(&name) {
        println!("Plugin '{}' is not installed.", name);
        return Ok(());
    }

    // Read the current config file and remove the plugin section
    let root_dir = config.root_dir()?;
    let config_path = root_dir.join("echo-system.toml");
    let content = std::fs::read_to_string(&config_path)?;

    // Parse and re-serialize without the plugin
    let mut doc: toml::Value = toml::from_str(&content)?;
    if let Some(plugins) = doc.get_mut("plugins").and_then(|v| v.as_table_mut()) {
        plugins.remove(&name);
        if plugins.is_empty() {
            if let Some(table) = doc.as_table_mut() {
                table.remove("plugins");
            }
        }
    }

    let new_content = toml::to_string_pretty(&doc)?;
    std::fs::write(&config_path, new_content)?;

    println!("Plugin '{}' removed from config.", name);
    println!(
        "Plugin data directory (plugins/{}) was not removed. Delete it manually if needed.",
        name
    );

    Ok(())
}
