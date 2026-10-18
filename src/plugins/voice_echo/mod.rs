pub(crate) mod brain;
pub mod config;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use super::{Plugin, PluginContext, PluginHealth, PluginMeta, PluginResult, SetupPrompt};
use crate::server::turns::{SlotWait, VOICE_CHANNEL};

const NAME: &str = "voice-echo";

/// How long an utterance waits for a free turn slot before the caller is
/// told the pulse is busy: long enough to ride out a burst, short enough
/// that the line does not go silent.
const SLOT_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Adapter wrapping the voice-echo crate. Calls run in-process: voice-echo
/// gets [`brain::TurnBrain`] as its LLM provider (its Local brain). With no
/// `bridge_url` it posts nothing to the daemon over HTTP — no transcript to
/// `/chat`, no `/session-started` or `/call-ended` notice — since every
/// such post in voice-echo 0.8.0 is gated on `llm.bridge_url`, which
/// [`factory_config`] strips.
pub struct VoiceEchoPlugin {
    inner: Option<Box<dyn pst05::plugin::Plugin>>,
    task: Option<tokio::task::JoinHandle<()>>,
    started: bool,
}

impl VoiceEchoPlugin {
    pub fn new() -> Self {
        Self {
            inner: None,
            task: None,
            started: false,
        }
    }
}

/// The plugin's config as voice-echo's factory reads it, minus the loopback
/// keys: with a `bridge_url` voice-echo would post every call to `/chat`
/// instead of using the brain it is given.
fn factory_config(
    raw: &toml::Value,
) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let mut json = serde_json::to_value(raw)
        .map_err(|e| format!("voice-echo: config is not representable as JSON: {e}"))?;
    // vendor-ok: voice-echo's config accepts this alias for its `llm` section.
    for section in ["llm", "claude"] {
        let removed = json
            .get_mut(section)
            .and_then(serde_json::Value::as_object_mut)
            .and_then(|table| table.remove("bridge_url"));
        if removed.is_some() {
            tracing::warn!(
                "{NAME}: `{section}.bridge_url` is ignored — calls run in-process now; remove it from the config"
            );
        }
    }
    Ok(json)
}

impl Plugin for VoiceEchoPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta {
            name: NAME.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: "Phone calls via Twilio (STT + TTS + voice pipeline)".to_string(),
        }
    }

    fn init<'a>(
        &'a mut self,
        toml_config: &'a toml::Value,
        ctx: &'a PluginContext,
    ) -> PluginResult<'a> {
        Box::pin(async move {
            let config = config::from_toml(toml_config)?;
            tracing::info!(
                "{NAME}: configured on {}:{}, calls run in-process as guests",
                config.server.host,
                config.server.port
            );
            let turns = ctx
                .turns
                .for_plugin(NAME, VOICE_CHANNEL)?
                .waiting_for_a_slot(SlotWait::Up(SLOT_WAIT));
            let factory_ctx = pst05::plugin::PluginContext {
                entity_root: ctx.pulse_root.clone(),
                entity_name: ctx.pulse_name.clone(),
                provider: Arc::new(brain::TurnBrain::new(turns)),
            };
            let voice = voice_echo::create(&factory_config(toml_config)?, &factory_ctx).await?;
            self.inner = Some(voice);
            Ok(())
        })
    }

    fn start(&mut self) -> PluginResult<'_> {
        Box::pin(async move {
            let mut inner = self.inner.take().ok_or("voice-echo: not initialized")?;
            let handle = tokio::spawn(async move {
                if let Err(e) = inner.start().await {
                    tracing::error!("voice-echo server exited with error: {e}");
                }
            });
            self.task = Some(handle);
            self.started = true;
            Ok(())
        })
    }

    fn stop(&mut self) -> PluginResult<'_> {
        Box::pin(async move {
            if let Some(task) = self.task.take() {
                task.abort();
            }
            self.started = false;
            Ok(())
        })
    }

    fn health(&self) -> Pin<Box<dyn Future<Output = PluginHealth> + Send + '_>> {
        Box::pin(async move {
            if self.started {
                PluginHealth::Healthy
            } else if self.inner.is_some() {
                PluginHealth::Degraded("initialized but not started".into())
            } else {
                PluginHealth::Down("not initialized".into())
            }
        })
    }

    fn setup_prompts(&self) -> Vec<SetupPrompt> {
        vec![
            SetupPrompt {
                key: "external_url".into(),
                question: "External URL (where Twilio can reach this server):".into(),
                required: true,
                secret: false,
                default: None,
            },
            SetupPrompt {
                key: "twilio_account_sid".into(),
                question: "Twilio Account SID:".into(),
                required: true,
                secret: false,
                default: None,
            },
            SetupPrompt {
                key: "twilio_auth_token".into(),
                question: "Twilio Auth Token:".into(),
                required: true,
                secret: true,
                default: None,
            },
            SetupPrompt {
                key: "twilio_phone_number".into(),
                question: "Twilio Phone Number (E.164 format, e.g. +1234567890):".into(),
                required: true,
                secret: false,
                default: None,
            },
        ]
    }

    fn platform_description(&self) -> Option<String> {
        Some(
            "Phone call integration via Twilio. Handles inbound and \
             outbound voice calls with speech-to-text and text-to-speech. \
             Callers reach you as guests: a phone number or a name on \
             the line proves nothing about who is speaking."
                .to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_returns_correct_info() {
        let plugin = VoiceEchoPlugin::new();
        let meta = plugin.meta();
        assert_eq!(meta.name, "voice-echo");
    }

    #[test]
    fn setup_prompts_not_empty() {
        let plugin = VoiceEchoPlugin::new();
        let prompts = plugin.setup_prompts();
        assert!(!prompts.is_empty());
    }

    #[tokio::test]
    async fn health_before_init_is_down() {
        let plugin = VoiceEchoPlugin::new();
        let health = plugin.health().await;
        assert!(matches!(health, PluginHealth::Down(_)));
    }

    #[test]
    fn factory_config_drops_bridge_url_under_either_section_name() {
        for section in ["llm", "claude"] {
            let mut llm = toml::value::Table::new();
            llm.insert("bridge_url".into(), "http://127.0.0.1:3200".into());
            llm.insert("max_response_tokens".into(), 256.into());
            let mut raw = toml::value::Table::new();
            raw.insert(section.into(), toml::Value::Table(llm));

            let json = factory_config(&toml::Value::Table(raw)).unwrap();
            assert!(json[section].get("bridge_url").is_none(), "{section}");
            assert_eq!(json[section]["max_response_tokens"], 256, "{section}");
        }
    }

    #[test]
    fn factory_config_without_bridge_url_is_unchanged() {
        let raw: toml::Value = toml::from_str("[llm]\nname = \"Echo\"\n").unwrap();
        let json = factory_config(&raw).unwrap();
        assert_eq!(json, serde_json::json!({"llm": {"name": "Echo"}}));
    }

    fn context(dir: &std::path::Path) -> PluginContext {
        PluginContext {
            pulse_root: dir.to_path_buf(),
            pulse_name: "Test".into(),
            provider: Arc::new(Box::new(crate::ollama_provider::OllamaProvider::new(
                "test".into(),
                Some("http://127.0.0.1:9".into()),
            ))),
            turns: crate::server::turns::TurnRunner::new(),
        }
    }

    fn voice_config() -> toml::Value {
        toml::Value::Table(toml::toml! {
            [server]
            host = "127.0.0.1"
            port = 0
            external_url = "https://example.com"

            [twilio]
            account_sid = "ACtest"
            auth_token = "token"
            phone_number = "+15551234567"

            [groq]
            api_key = "gsk_test"

            [inworld]
            api_key = "iw_test"

            [claude]
            bridge_url = "http://127.0.0.1:3200"

            [vad]
        })
    }

    /// Init builds the voice server in-process without starting it.
    #[tokio::test]
    async fn init_builds_the_voice_server_without_starting_it() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let mut plugin = VoiceEchoPlugin::new();
        plugin.init(&voice_config(), &ctx).await.unwrap();
        assert!(matches!(plugin.health().await, PluginHealth::Degraded(_)));
    }

    #[tokio::test]
    async fn init_refuses_an_incomplete_config() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let mut plugin = VoiceEchoPlugin::new();
        let incomplete = toml::Value::Table(toml::toml! {
            [server]
            host = "127.0.0.1"
        });
        let err = plugin.init(&incomplete, &ctx).await.unwrap_err();
        assert!(
            err.to_string().contains("invalid voice-echo config"),
            "{err}"
        );
        assert!(matches!(plugin.health().await, PluginHealth::Down(_)));
    }
}
