use super::Plugin;

/// A known plugin in the registry
#[derive(Debug, Clone)]
pub struct RegistryEntry {
    pub name: String,
    pub description: String,
    pub version: String,
    pub available: bool,
}

/// Return the list of known plugins (hardcoded for now)
pub fn known_plugins() -> Vec<RegistryEntry> {
    vec![
        RegistryEntry {
            name: "voice-echo".to_string(),
            description: "Phone calls via Twilio (STT + TTS + voice pipeline)".to_string(),
            version: "0.1.0".to_string(),
            available: false, // Not yet implemented
        },
        RegistryEntry {
            name: "discord-echo".to_string(),
            description: "Discord bot presence and voice channels".to_string(),
            version: "0.1.0".to_string(),
            available: false,
        },
        RegistryEntry {
            name: "n8n-integration".to_string(),
            description: "Complex workflow automation via n8n".to_string(),
            version: "0.1.0".to_string(),
            available: false,
        },
    ]
}

/// Look up a plugin in the registry
pub fn find_plugin(name: &str) -> Option<RegistryEntry> {
    known_plugins().into_iter().find(|e| e.name == name)
}

/// Factory function: create a plugin instance by name.
/// Returns None if the plugin is not known or not yet implemented.
pub fn create_plugin(name: &str) -> Option<Box<dyn Plugin>> {
    // Plugins will be added here as they're implemented:
    // "voice-echo" => Some(Box::new(voice_echo::VoiceEchoPlugin::new())),
    // "discord-echo" => Some(Box::new(discord_echo::DiscordEchoPlugin::new())),
    let _ = name; // suppress unused warning when no plugins exist
    tracing::debug!("Plugin '{}' is not yet available", name);
    None
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
    fn test_create_plugin_not_available() {
        // Known plugins exist but aren't implemented yet
        let plugin = create_plugin("voice-echo");
        assert!(plugin.is_none());
    }

    #[test]
    fn test_create_unknown_plugin() {
        let plugin = create_plugin("totally-unknown");
        assert!(plugin.is_none());
    }
}
