//! Tests of the vendor-blind runner. The adapter used is the Claude one
//! because its wire format is the simplest to fake; nothing here depends on
//! Claude beyond that.

use std::path::{Path, PathBuf};

use pulse_system_types::llm::{LmProvider, Message, MessageContent, MessageSource, Role};

use super::adapter::{CliAdapter, Invocation};
use super::adapters::claude::Claude;
use super::adapters::codex::Codex;
use super::adapters::grok::Grok;
use super::*;
use crate::streaming::{StreamEvent, StreamingProvider};
use tokio_stream::StreamExt;

fn user(text: &str) -> Message {
    Message {
        role: Role::User,
        content: MessageContent::Text(text.into()),
        source: None,
    }
}

// --- serialize_messages ---

#[test]
fn serialize_single_user_message() {
    let out = serialize_messages(&[user("hello")]);
    assert!(out.starts_with("[User]: hello"), "{out}");
    assert!(out.ends_with("\n\n[Assistant]:"));
}

#[test]
fn serialize_alternates_roles() {
    let msgs = [
        user("q1"),
        Message {
            role: Role::Assistant,
            content: MessageContent::Text("a1".into()),
            source: None,
        },
        user("q2"),
    ];
    let out = serialize_messages(&msgs);
    assert!(
        out.contains("[User]: q1\n\n[Assistant]: a1\n\n[User]: q2"),
        "{out}"
    );
}

#[test]
fn serialize_marks_scheduled_tasks() {
    let msg = Message {
        role: Role::User,
        content: MessageContent::Text("run".into()),
        source: Some(MessageSource::ScheduledTask {
            task_name: "t".into(),
        }),
    };
    assert!(serialize_messages(&[msg]).starts_with("[Task]: run"));
}

// --- command anchoring ---

#[test]
fn entity_command_sets_cwd_env_and_scrubs() {
    let root = tempfile::tempdir().unwrap();
    let cmd = entity_command("/usr/bin/true", root.path(), &Claude);
    let std_cmd = cmd.as_std();
    assert_eq!(std_cmd.get_current_dir(), Some(root.path()));
    let envs: std::collections::HashMap<_, _> = std_cmd.get_envs().collect();
    assert_eq!(
        envs.get(std::ffi::OsStr::new(RECALL_ECHO_HOME)),
        Some(&Some(root.path().as_os_str()))
    );
    // The child starts from a cleared environment; a vendor variable that
    // is not this adapter's never appears, and CLAUDECODE is scrubbed even
    // though it carries this adapter's prefix.
    assert!(!envs.contains_key(std::ffi::OsStr::new("CLAUDECODE")));
    assert!(cmd
        .as_std()
        .get_envs()
        .all(|(k, _)| k != "GROK_REASONING_EFFORT"));
}

#[test]
fn provider_resolves_bin_from_override_then_default() {
    let root = tempfile::tempdir().unwrap();
    let p = CliProvider::new(
        Box::new(Claude),
        Some("/x/claude".into()),
        "m".into(),
        root.path().into(),
    );
    assert_eq!(p.bin(), "/x/claude");
    assert_eq!(p.entity_root(), root.path());
    assert_eq!(p.adapter_name(), "claude");
    assert_eq!(p.name(), "cli");
    assert!(!p.supports_tools());
}

// --- staging ---

#[test]
fn staged_file_is_private_and_self_cleaning() {
    let f = StagedFile::create("t", "hello").unwrap();
    let path = f.path().to_path_buf();
    assert!(path.exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    drop(f);
    assert!(!path.exists(), "guard must unlink the file when dropped");
}

#[test]
fn stage_prepends_when_the_cli_has_no_system_prompt_channel() {
    struct Prepending;
    impl CliAdapter for Prepending {
        fn name(&self) -> &'static str {
            "prep"
        }
        fn default_bin(&self) -> &'static str {
            "prep"
        }
        fn default_model(&self) -> &'static str {
            ""
        }
        fn prompt_delivery(&self) -> adapter::PromptDelivery {
            adapter::PromptDelivery::Stdin
        }
        fn system_prompt_delivery(&self) -> adapter::SystemPromptDelivery {
            adapter::SystemPromptDelivery::Prepend
        }
        fn output_mode(&self, _: bool) -> adapter::OutputMode {
            adapter::OutputMode::SingleJson
        }
        fn invoke_args(&self, _: &Invocation<'_>) -> Vec<std::ffi::OsString> {
            vec![]
        }
        fn parse_single(&self, _: &str) -> Result<adapter::Reply, String> {
            Err("n/a".into())
        }
        fn parse_stream_line(&self, _: &str) -> adapter::StreamLine {
            adapter::StreamLine::Other
        }
        fn classify_exit(&self, _: &str, _: &str) -> adapter::ExitClass {
            adapter::ExitClass::Plain
        }
        fn integration(&self) -> &dyn adapter::AgentIntegration {
            unimplemented!()
        }
    }
    let staged = stage(&Prepending, "SYS", "USER").unwrap();
    assert!(staged.system_prompt_file.is_none());
    assert_eq!(staged.stdin_text.as_deref(), Some("SYS\n\nUSER"));
}

#[test]
fn stage_refuses_an_oversized_argv_system_prompt() {
    struct Argv;
    impl CliAdapter for Argv {
        fn name(&self) -> &'static str {
            "argv"
        }
        fn default_bin(&self) -> &'static str {
            "argv"
        }
        fn default_model(&self) -> &'static str {
            ""
        }
        fn prompt_delivery(&self) -> adapter::PromptDelivery {
            adapter::PromptDelivery::File
        }
        fn system_prompt_delivery(&self) -> adapter::SystemPromptDelivery {
            adapter::SystemPromptDelivery::Argv { max_bytes: 10 }
        }
        fn output_mode(&self, _: bool) -> adapter::OutputMode {
            adapter::OutputMode::Ndjson
        }
        fn invoke_args(&self, _: &Invocation<'_>) -> Vec<std::ffi::OsString> {
            vec![]
        }
        fn parse_single(&self, _: &str) -> Result<adapter::Reply, String> {
            Err("n/a".into())
        }
        fn parse_stream_line(&self, _: &str) -> adapter::StreamLine {
            adapter::StreamLine::Other
        }
        fn classify_exit(&self, _: &str, _: &str) -> adapter::ExitClass {
            adapter::ExitClass::Plain
        }
        fn integration(&self) -> &dyn adapter::AgentIntegration {
            unimplemented!()
        }
    }
    assert!(stage(&Argv, &"x".repeat(11), "p").is_err());
    let ok = stage(&Argv, "short", "p").unwrap();
    assert!(ok.prompt_file.is_some());
    assert!(ok.stdin_text.is_none());
}

// --- NDJSON reduction ---

#[test]
fn ndjson_precedence_follows_the_adapter() {
    let body = concat!(
        r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"text":"na"}}}"#,
        "\n",
        r#"{"type":"result","result":"narration","is_error":false}"#,
        "\n",
        r#"{"type":"result","result":"final","is_error":false,"usage":{"input_tokens":1,"output_tokens":2}}"#,
        "\n",
    );
    // Grok's deltas span every turn: the last record is the answer.
    let reply = reply_from_ndjson(&Grok, body).unwrap();
    assert_eq!(reply.text, "final");
    assert_eq!(reply.usage.output_tokens, Some(2));
    // Claude keeps its pre-PN-106 behaviour: assembled deltas first.
    assert_eq!(reply_from_ndjson(&Claude, body).unwrap().text, "na");

    let deltas_only =
        r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"text":"only"}}}"#;
    assert_eq!(reply_from_ndjson(&Grok, deltas_only).unwrap().text, "only");

    let err = r#"{"type":"result","result":"quota","is_error":true}"#;
    assert_eq!(reply_from_ndjson(&Claude, err).unwrap_err(), "quota");
    assert!(reply_from_ndjson(&Claude, "").is_err());
}

// --- end to end against a fake CLI ---

/// A stand-in binary: records argv and the prompt it received, answers with
/// a fixed JSON document.
struct MockCli {
    _dir: tempfile::TempDir,
    bin: PathBuf,
    argv_log: PathBuf,
    prompt_copy: PathBuf,
}

impl MockCli {
    fn new(reply_cmd: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("fake-cli");
        let argv_log = dir.path().join("argv.log");
        let prompt_copy = dir.path().join("prompt.txt");
        let script = format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" > '{}'\ncat > '{}'\n{}\n",
            argv_log.display(),
            prompt_copy.display(),
            reply_cmd
        );
        std::fs::write(&bin, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Self {
            _dir: dir,
            bin,
            argv_log,
            prompt_copy,
        }
    }

    fn succeeding() -> Self {
        Self::new("printf '{\"result\":\"mock ok\",\"session_id\":\"mock\"}'")
    }

    fn provider(&self, model: &str) -> CliProvider {
        self.provider_for(Box::new(Claude), model)
    }

    fn provider_for(&self, adapter: Box<dyn CliAdapter>, model: &str) -> CliProvider {
        CliProvider::new(
            adapter,
            Some(self.bin.display().to_string()),
            model.into(),
            self._dir.path().to_path_buf(),
        )
    }

    fn argv(&self) -> Vec<String> {
        std::fs::read_to_string(&self.argv_log)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

#[tokio::test]
async fn system_prompt_goes_to_a_file_not_argv_and_is_removed_after() {
    let mock = MockCli::succeeding();
    let big = "S".repeat(200_000);
    let out = mock
        .provider("m")
        .invoke(&big, &[user("hi")], 10, None)
        .await
        .expect("mock succeeds");
    assert!(
        matches!(&out.content[0], pulse_system_types::llm::ContentBlock::Text { text } if text == "mock ok")
    );
    let argv = mock.argv();
    let idx = argv
        .iter()
        .position(|a| a == "--system-prompt-file")
        .unwrap();
    let path = Path::new(&argv[idx + 1]);
    assert!(
        !argv.iter().any(|a| a.len() > 10_000),
        "prompt never on argv"
    );
    assert!(!path.exists(), "staged file unlinked after invocation");
    assert!(std::fs::read_to_string(&mock.prompt_copy)
        .unwrap()
        .contains("[User]: hi"));
}

#[tokio::test]
async fn staged_file_is_removed_when_the_cli_fails() {
    let mock = MockCli::new("echo boom >&2; exit 3");
    let err = mock
        .provider("m")
        .invoke("sys", &[user("x")], 10, None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("boom"), "{err}");
    let argv = mock.argv();
    let idx = argv
        .iter()
        .position(|a| a == "--system-prompt-file")
        .unwrap();
    assert!(!Path::new(&argv[idx + 1]).exists());
}

#[tokio::test]
async fn unsupported_cli_fails_with_a_named_error() {
    // A CLI that does not know --system-prompt-file says so during parsing.
    let mock = MockCli::new("echo \"error: unknown option '--system-prompt-file'\" >&2; exit 1");
    let err = mock
        .provider("m")
        .invoke("sys", &[user("x")], 10, None)
        .await
        .unwrap_err();
    let text = err.to_string();
    assert!(
        text.contains("claude") && text.contains("--system-prompt-file"),
        "{text}"
    );
}

mod timeout_tests {
    use super::super::subprocess_timeout;

    #[test]
    fn default_timeout_is_fifteen_minutes() {
        std::env::remove_var("PULSE_LLM_TIMEOUT_SECS");
        std::env::remove_var("RECALL_LLM_TIMEOUT_SECS");
        assert_eq!(subprocess_timeout().as_secs(), 900);
    }
}

// --- streaming precedence and non-default deliveries, end to end ---

/// A CLI whose stream carries narration deltas and two result records; the
/// grok adapter must answer with the last record, never the deltas.
#[tokio::test]
async fn streaming_prefers_the_terminal_record_when_the_adapter_says_so() {
    let mock = MockCli::new(concat!(
        "printf '%s\\n' ",
        "'{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"text\":\"I will check\"}}}' ",
        "'{\"type\":\"result\",\"result\":\"narration\",\"is_error\":false}' ",
        "'{\"type\":\"result\",\"result\":\"final answer\",\"is_error\":false,\"usage\":{\"input_tokens\":7,\"output_tokens\":3}}'"
    ));
    let provider = mock.provider_for(Box::new(Grok), "grok-x");
    let mut stream = provider.invoke_streaming("sys", &[user("hi")], 10, None);
    let mut done = None;
    while let Some(event) = stream.next().await {
        if let StreamEvent::Done(reply) = event {
            done = Some(reply);
        }
    }
    let reply = done.expect("stream completes");
    assert!(
        matches!(&reply.content[0], pulse_system_types::llm::ContentBlock::Text { text } if text == "final answer"),
        "{reply:?}"
    );
    assert_eq!(reply.output_tokens, Some(3));
    let argv = mock.argv();
    assert!(argv.contains(&"--prompt-file".to_string()));
    assert!(argv.contains(&"--system-prompt-override".to_string()));
    assert!(!argv.iter().any(|a| a == "-"), "grok never reads stdin");
    // The buffered path agrees.
    let out = mock
        .provider_for(Box::new(Grok), "grok-x")
        .invoke("sys", &[user("hi")], 10, None)
        .await
        .unwrap();
    assert!(
        matches!(&out.content[0], pulse_system_types::llm::ContentBlock::Text { text } if text == "final answer")
    );
}

/// Codex takes the prompt on stdin with the system prompt prepended and
/// reports usage on a separate line.
#[tokio::test]
async fn codex_gets_a_prepended_stdin_prompt_and_usage_is_folded() {
    let mock = MockCli::new(concat!(
        "printf '%s\\n' ",
        "'{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"codex says hi\"}}' ",
        "'{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":11,\"output_tokens\":2}}'"
    ));
    let out = mock
        .provider_for(Box::new(Codex), "")
        .invoke("SYSTEM RULES", &[user("hello")], 10, None)
        .await
        .unwrap();
    assert!(
        matches!(&out.content[0], pulse_system_types::llm::ContentBlock::Text { text } if text == "codex says hi")
    );
    assert_eq!(out.input_tokens, Some(11));
    let prompt = std::fs::read_to_string(&mock.prompt_copy).unwrap();
    assert!(prompt.starts_with("SYSTEM RULES\n\n"), "{prompt}");
    assert!(prompt.contains("[User]: hello"));
    let argv = mock.argv();
    assert_eq!(argv[0], "exec");
    assert!(!argv.iter().any(|a| a == "-m"), "empty model omits -m");
}

/// A policy refusal surfaces as a downcastable `RefusalError`.
#[tokio::test]
async fn refusal_is_a_typed_error() {
    let mock = MockCli::new(
        "printf '{\"type\":\"result\",\"is_error\":true,\"result\":\"This violates the Usage Policy\"}'; exit 1",
    );
    let err = mock
        .provider("m")
        .invoke("sys", &[user("x")], 10, None)
        .await
        .unwrap_err();
    assert!(
        err.downcast_ref::<crate::errors::RefusalError>().is_some(),
        "{err}"
    );
}

#[test]
fn env_set_reaches_the_child() {
    let root = tempfile::tempdir().unwrap();
    let cmd = entity_command("/usr/bin/true", root.path(), &Grok);
    let envs: std::collections::HashMap<_, _> = cmd.as_std().get_envs().collect();
    assert_eq!(
        envs.get(std::ffi::OsStr::new("RUST_LOG")),
        Some(&Some(std::ffi::OsStr::new("warn")))
    );
}
