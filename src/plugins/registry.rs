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
            available: cfg!(feature = "voice"),
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
    match name {
        #[cfg(feature = "voice")]
        "voice-echo" => Some(Box::new(super::voice_echo::VoiceEchoPlugin::new())),
        _ => {
            tracing::debug!("Plugin '{}' is not available", name);
            None
        }
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
    fn test_create_voice_echo_plugin() {
        let plugin = create_plugin("voice-echo");
        if cfg!(feature = "voice") {
            assert!(plugin.is_some());
            assert_eq!(plugin.unwrap().meta().name, "voice-echo");
        } else {
            assert!(plugin.is_none());
        }
    }

    #[test]
    fn test_create_unknown_plugin() {
        let plugin = create_plugin("totally-unknown");
        assert!(plugin.is_none());
    }

    #[test]
    fn test_voice_echo_availability_matches_feature() {
        let entry = find_plugin("voice-echo").unwrap();
        assert_eq!(entry.available, cfg!(feature = "voice"));
    }
}
