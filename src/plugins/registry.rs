use super::Plugin;

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
            name: "voice-echo".to_string(),
            description: "Phone calls via Twilio (STT + TTS + voice pipeline)".to_string(),
            version: "0.6.0".to_string(),
            available: cfg!(feature = "voice"),
        },
        RegistryEntry {
            name: "chat-echo".to_string(),
            description: "Web chat UI for echo-system".to_string(),
            version: "0.1.0".to_string(),
            available: cfg!(feature = "chat"),
        },
        RegistryEntry {
            name: "discord-echo".to_string(),
            description: "Discord bot presence and voice channels".to_string(),
            version: "0.1.0".to_string(),
            available: false, // blocked on songbird dep fix
        },
        RegistryEntry {
            name: "bridge-echo".to_string(),
            description: "HTTP bridge for Claude Code integration".to_string(),
            version: "0.2.0".to_string(),
            available: cfg!(feature = "bridge"),
        },
        RegistryEntry {
            name: "recall-echo".to_string(),
            description: "Three-layer persistent memory system".to_string(),
            version: "0.6.0".to_string(),
            available: cfg!(feature = "recall"),
        },
        RegistryEntry {
            name: "praxis-echo".to_string(),
            description: "Pipeline enforcement and behavioral policies".to_string(),
            version: "0.1.0".to_string(),
            available: cfg!(feature = "praxis"),
        },
        RegistryEntry {
            name: "vigil-echo".to_string(),
            description: "Metacognitive monitoring and signal tracking".to_string(),
            version: "0.1.0".to_string(),
            available: cfg!(feature = "vigil"),
        },
    ]
}

/// Look up a plugin in the registry
pub fn find_plugin(name: &str) -> Option<RegistryEntry> {
    known_plugins().into_iter().find(|e| e.name == name)
}

/// Factory function: create a plugin instance by name.
/// Returns None if the plugin is not compiled in or not yet implemented.
pub fn create_plugin(name: &str) -> Option<Box<dyn Plugin>> {
    match name {
        #[cfg(feature = "voice")]
        "voice-echo" => Some(Box::new(super::voice_echo::VoiceEchoPlugin::new())),
        #[cfg(feature = "chat")]
        "chat-echo" => Some(Box::new(super::chat_echo::ChatEchoPlugin::new())),
        // "discord-echo" blocked on songbird dep fix
        #[cfg(feature = "bridge")]
        "bridge-echo" => Some(Box::new(super::bridge_echo::BridgeEchoPlugin::new())),
        #[cfg(feature = "recall")]
        "recall-echo" => Some(Box::new(super::recall_echo::RecallEchoPlugin::new())),
        #[cfg(feature = "praxis")]
        "praxis-echo" => Some(Box::new(super::praxis_echo::PraxisEchoPlugin::new())),
        #[cfg(feature = "vigil")]
        "vigil-echo" => Some(Box::new(super::vigil_echo::VigilEchoPlugin::new())),
        _ => {
            tracing::debug!("Plugin '{name}' is not available");
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
    fn test_all_known_plugins_registered() {
        let plugins = known_plugins();
        let names: Vec<&str> = plugins.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"voice-echo"));
        assert!(names.contains(&"chat-echo"));
        assert!(names.contains(&"discord-echo"));
        assert!(names.contains(&"bridge-echo"));
        assert!(names.contains(&"recall-echo"));
        assert!(names.contains(&"praxis-echo"));
        assert!(names.contains(&"vigil-echo"));
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
