use super::{Plugin, PluginContext};

/// A known plugin in the registry
#[derive(Debug, Clone)]
pub struct RegistryEntry {
    pub name: String,
    pub description: String,
    pub version: String,
    pub available: bool,
}

/// Return the list of known plugins
pub fn known_plugins() -> Vec<RegistryEntry> {
    vec![
        RegistryEntry {
            name: "recall-echo".to_string(),
            description: "Three-layer persistent memory system".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            available: true,
        },
        RegistryEntry {
            name: "praxis-echo".to_string(),
            description: "Pipeline enforcement and behavioral policies".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            available: true,
        },
        RegistryEntry {
            name: "pulse-echo".to_string(),
            description: "Operational self-model and outcome tracking".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            available: true,
        },
        RegistryEntry {
            name: "vigil-echo".to_string(),
            description: "Metacognitive monitoring and signal tracking".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            available: true,
        },
        RegistryEntry {
            name: "bridge-echo".to_string(),
            description: "HTTP bridge for Claude Code integration".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            available: true,
        },
        RegistryEntry {
            name: "chat-echo".to_string(),
            description: "Web chat UI for pulse-null".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            available: true,
        },
        RegistryEntry {
            name: "voice-echo".to_string(),
            description: "Phone calls via Twilio (STT + TTS + voice pipeline)".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            available: cfg!(feature = "voice"),
        },
        RegistryEntry {
            name: "discord-echo".to_string(),
            description: "Discord bot presence and voice channels".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            available: cfg!(feature = "discord"),
        },
        RegistryEntry {
            name: "discord-text-echo".to_string(),
            description: "Discord text channels (read + write)".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            available: cfg!(feature = "discord-text"),
        },
    ]
}

/// Look up a plugin in the registry
pub fn find_plugin(name: &str) -> Option<RegistryEntry> {
    known_plugins().into_iter().find(|e| e.name == name)
}

/// Factory function: create a plugin instance by name.
///
/// Converts TOML config to JSON before passing to the crate's factory.
/// Returns Ok(None) if the plugin is not compiled in.
pub async fn create_plugin(
    name: &str,
    toml_config: &toml::Value,
    ctx: &PluginContext,
) -> Result<Option<Box<dyn Plugin>>, Box<dyn std::error::Error + Send + Sync>> {
    // Convert TOML value to JSON for format-agnostic factories
    let json_str = serde_json::to_string(toml_config)?;
    let json_config: serde_json::Value = serde_json::from_str(&json_str)?;

    match name {
        // Core plugins (always available)
        "recall-echo" => Ok(Some(recall_echo::create(&json_config, ctx).await?)),
        "praxis-echo" => Ok(Some(praxis_echo::create(&json_config, ctx).await?)),
        "pulse-echo" => Ok(Some(pulse_echo::create(&json_config, ctx).await?)),
        "vigil-echo" => Ok(Some(vigil_echo::create(&json_config, ctx).await?)),
        "bridge-echo" => Ok(Some(bridge_echo::create(&json_config, ctx).await?)),
        "chat-echo" => Ok(Some(chat_echo::create(&json_config, ctx).await?)),
        // Optional plugins (feature-gated)
        #[cfg(feature = "voice")]
        "voice-echo" => Ok(Some(voice_echo::create(&json_config, ctx).await?)),
        #[cfg(feature = "discord")]
        "discord-echo" => Ok(Some(discord_voice_echo::create(&json_config, ctx).await?)),
        #[cfg(feature = "discord-text")]
        "discord-text-echo" => Ok(Some(discord_echo::create(&json_config, ctx).await?)),
        _ => {
            tracing::debug!("Plugin '{name}' is not available");
            Ok(None)
        }
    }
}

/// Get setup prompts for a plugin by name, without requiring full construction.
///
/// Creates a plugin with empty config to retrieve its setup prompts.
/// Used by the init wizard before config exists.
pub async fn setup_prompts_for(
    name: &str,
    ctx: &PluginContext,
) -> Option<Vec<echo_system_types::SetupPrompt>> {
    let empty_config = toml::Value::Table(toml::value::Table::new());
    // Try to create with empty config — core plugins handle defaults gracefully.
    // Optional plugins may fail (missing required fields), which is fine.
    match create_plugin(name, &empty_config, ctx).await {
        Ok(Some(plugin)) => {
            let prompts = plugin.setup_prompts();
            if prompts.is_empty() {
                None
            } else {
                Some(prompts)
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_known_plugins_not_empty() {
        let plugins = known_plugins();
        assert!(!plugins.is_empty());
        assert!(plugins.iter().any(|p| p.name == "voice-echo"));
    }

    #[test]
    fn test_find_known_plugin() {
        let entry = find_plugin("voice-echo");
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().name, "voice-echo");
    }

    #[test]
    fn test_find_unknown_plugin() {
        let entry = find_plugin("does-not-exist");
        assert!(entry.is_none());
    }

    #[test]
    fn test_all_known_plugins_registered() {
        let plugins = known_plugins();
        let names: Vec<&str> = plugins.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"voice-echo"));
        assert!(names.contains(&"chat-echo"));
        assert!(names.contains(&"discord-echo"));
        assert!(names.contains(&"bridge-echo"));
        assert!(names.contains(&"recall-echo"));
        assert!(names.contains(&"praxis-echo"));
        assert!(names.contains(&"pulse-echo"));
        assert!(names.contains(&"vigil-echo"));
        assert!(names.contains(&"discord-text-echo"));
    }

    #[test]
    fn test_voice_echo_availability_matches_feature() {
        let entry = find_plugin("voice-echo").unwrap();
        assert_eq!(entry.available, cfg!(feature = "voice"));
    }
}
