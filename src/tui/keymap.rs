//! One table of key bindings per context. The bottom hint line and the `?`
//! help float are both generated from it, so they cannot disagree.

/// A key (or chord) and what it does, in the user's words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub keys: &'static str,
    pub what: &'static str,
}

const fn b(keys: &'static str, what: &'static str) -> Binding {
    Binding { keys, what }
}

/// Where the keys land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Context {
    Boot,
    /// Talk, prompt focused.
    Prompt {
        turn_active: bool,
    },
    /// Talk, transcript focused.
    Transcript,
    CmdLine,
    Confirm,
    Help,
}

/// Chords that work everywhere a page is showing.
pub const GLOBAL: &[Binding] = &[
    b("Ctrl+h/j/k/l", "move focus between panes"),
    b(":", "command line"),
    b("?", "keys for the focused pane"),
];

/// Every binding for `ctx`, in the order the help shows them.
#[must_use]
pub fn bindings(ctx: Context) -> Vec<Binding> {
    match ctx {
        Context::Boot => vec![b("q", "quit")],
        Context::Prompt { turn_active } => vec![
            b("Enter", "send (or queue one while a reply streams)"),
            b("Shift+Enter / Alt+Enter", "newline"),
            if turn_active {
                b("Ctrl+c", "cancel the reply")
            } else {
                b("Ctrl+c", "quit")
            },
            b("Ctrl+d", "quit (empty prompt)"),
            b("Esc", "focus the transcript"),
            b("↑ / ↓", "history when the prompt is one line"),
            b("Ctrl+u / Ctrl+w", "delete to line start / word"),
            b("PgUp / PgDn", "scroll the transcript"),
        ],
        Context::Transcript => vec![
            b("j / k", "scroll"),
            b("g / G", "top / follow the tail"),
            b("Ctrl+u / Ctrl+d", "half page"),
            b("i / Enter", "focus the prompt"),
            b("f", "fullscreen this pane"),
            b("q", "quit"),
        ],
        Context::CmdLine => vec![b("Tab", "complete"), b("Enter", "run"), b("Esc", "close")],
        Context::Confirm => vec![b("Enter", "confirm"), b("Esc", "cancel")],
        Context::Help => vec![b("Esc / ?", "close")],
    }
}

/// The short version for the bottom line: the first `n` bindings, keys
/// abbreviated where the long form would not fit.
#[must_use]
pub fn hints(ctx: Context, n: usize) -> Vec<(&'static str, &'static str)> {
    bindings(ctx)
        .into_iter()
        .take(n)
        .map(|b| (short_keys(b.keys), short_what(b.what)))
        .collect()
}

fn short_keys(keys: &'static str) -> &'static str {
    match keys {
        "Shift+Enter / Alt+Enter" => "Shift+Enter",
        "Ctrl+h/j/k/l" => "Ctrl+hjkl",
        "Ctrl+u / Ctrl+w" => "Ctrl+u/w",
        "↑ / ↓" => "↑↓",
        other => other,
    }
}

fn short_what(what: &'static str) -> &'static str {
    match what {
        "send (or queue one while a reply streams)" => "send",
        "cancel the reply" => "cancel",
        "quit (empty prompt)" => "quit",
        "focus the transcript" => "transcript",
        "history when the prompt is one line" => "history",
        "delete to line start / word" => "kill",
        "scroll the transcript" => "scroll",
        "top / follow the tail" => "top/tail",
        "half page" => "half page",
        "focus the prompt" => "prompt",
        "fullscreen this pane" => "fullscreen",
        "move focus between panes" => "focus",
        "keys for the focused pane" => "keys",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_are_a_prefix_of_the_bindings() {
        for ctx in [
            Context::Boot,
            Context::Prompt { turn_active: false },
            Context::Prompt { turn_active: true },
            Context::Transcript,
            Context::CmdLine,
            Context::Confirm,
            Context::Help,
        ] {
            let all = bindings(ctx);
            let h = hints(ctx, 4);
            assert_eq!(h.len(), all.len().min(4));
            for (i, (k, _)) in h.iter().enumerate() {
                assert_eq!(*k, short_keys(all[i].keys), "{ctx:?} hint {i}");
            }
        }
    }

    #[test]
    fn prompt_ctrl_c_meaning_follows_the_turn() {
        let idle = bindings(Context::Prompt { turn_active: false });
        let busy = bindings(Context::Prompt { turn_active: true });
        assert!(idle.iter().any(|b| b.keys == "Ctrl+c" && b.what == "quit"));
        assert!(busy
            .iter()
            .any(|b| b.keys == "Ctrl+c" && b.what == "cancel the reply"));
    }

    #[test]
    fn no_context_has_duplicate_keys() {
        for ctx in [
            Context::Prompt { turn_active: false },
            Context::Transcript,
            Context::CmdLine,
        ] {
            let all = bindings(ctx);
            let mut keys: Vec<&str> = all.iter().map(|b| b.keys).collect();
            keys.sort_unstable();
            keys.dedup();
            assert_eq!(keys.len(), all.len(), "{ctx:?}");
        }
    }
}
