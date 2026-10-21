//! `pulse-null probe verify-isolation`: evidence that a probe item writes
//! nowhere live.
//!
//! Two parts. The sandbox self-test spends no model turn: a shell started
//! exactly as a probe's CLI child is (same env, same Landlock sandbox, same
//! working copy) tries to write a marker into the live pulse home, the live
//! user's home and the live vendor directory, and into its own copy; to
//! read every path on the read fence's deny-list (the live home, its
//! `secrets/` and `record/`, the graph credential directory, `~/.config`,
//! `~/.ssh`, the live vendor directory); and to connect to the pulse's own
//! port and the live graph server's. Every live write, read and connect must
//! be refused by the kernel; the write into the copy and a read of a system
//! file must succeed. The optional
//! canned turn then runs one real CLI turn in the copy and checks that the
//! CLI's transcript landed in the scratch vendor directory and that nothing
//! in the live vendor directory names the scratch.

use std::path::{Path, PathBuf};

use crate::init::agent_bootstrap::shell_quote;

use super::fence::DenyList;
use super::scratch::ItemScratch;

/// One isolation check and what it found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

impl Check {
    fn new(name: impl Into<String>, passed: bool, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            passed,
            detail: detail.into(),
        }
    }
}

/// The live locations a probe item must not write to.
#[derive(Debug, Clone)]
pub struct LiveTargets<'a> {
    pub live_root: &'a Path,
    pub live_home: &'a Path,
    /// The live user's vendor directory for the pulse's CLI, if it has one.
    pub vendor_home: Option<&'a Path>,
    /// The live pulse's own HTTP port.
    pub pulse_port: u16,
    /// The live graph server's port, when the pulse's graph is on one.
    pub graph_port: Option<u16>,
    /// The paths the read fence must keep unreadable.
    pub deny: &'a DenyList,
    /// Ports the child's sandbox allows (443) that the probe unit must
    /// still refuse on 127.0.0.1 (the host's nginx); empty to skip.
    pub unit_ports: &'a [u16],
}

/// What the self-test shell expects at one path.
struct WriteProbe {
    label: String,
    dir: PathBuf,
    expect_write: bool,
}

/// Run the sandbox self-test for `item` against `targets`.
pub async fn sandbox_self_test(
    item: &ItemScratch,
    targets: &LiveTargets<'_>,
    marker: &str,
) -> Vec<Check> {
    let test = SelfTest {
        writes: write_probes(item, targets),
        reads: read_probes(item, targets.deny),
        ports: connect_probes(targets),
        bash: Path::new("/bin/bash").exists(),
    };
    match run_in_sandbox(item, test.script(marker)).await {
        Ok(stdout) => test.checks(&stdout, marker),
        Err(failed) => vec![failed],
    }
}

/// Everything one self-test asks its shell to try.
struct SelfTest {
    writes: Vec<WriteProbe>,
    reads: Vec<ReadProbe>,
    ports: Vec<(&'static str, u16)>,
    /// The connect probes need `/bin/bash` (its `/dev/tcp`).
    bash: bool,
}

impl SelfTest {
    /// The shell script: one line per probe, each printing what happened.
    fn script(&self, marker: &str) -> String {
        let mut script = self.write_lines(marker);
        script.push_str(&self.read_lines());
        if self.bash {
            script.push_str(&self.connect_lines());
        }
        script
    }

    fn write_lines(&self, marker: &str) -> String {
        let mut lines = String::new();
        for (index, probe) in self.writes.iter().enumerate() {
            let path = shell_quote(&probe.dir.join(marker));
            lines.push_str(&format!(
                "if ( : > {path} ) 2>/dev/null; then echo 'W {index}'; rm -f {path}; else echo 'D {index}'; fi\n"
            ));
        }
        lines
    }

    fn read_lines(&self) -> String {
        let mut lines = String::new();
        for (index, probe) in self.reads.iter().enumerate() {
            let path = shell_quote(&probe.path);
            let read = if probe.path.is_dir() {
                format!("ls -A {path}")
            } else {
                format!("head -c 1 {path}")
            };
            lines.push_str(&format!(
                "if {read} >/dev/null 2>&1; then echo 'R {index}'; else echo 'U {index}'; fi\n"
            ));
        }
        lines
    }

    fn connect_lines(&self) -> String {
        let mut lines = String::new();
        for (index, (_, port)) in self.ports.iter().enumerate() {
            lines.push_str(&format!(
                "/bin/bash -c 'exec 3<>/dev/tcp/127.0.0.1/{port}' 2>&1 | sed 's/^/T{index} /'\n"
            ));
        }
        lines
    }

    /// The checks, in probe order, from what the shell printed.
    fn checks(&self, stdout: &str, marker: &str) -> Vec<Check> {
        let mut checks: Vec<Check> = self
            .writes
            .iter()
            .enumerate()
            .map(|(index, probe)| write_check(probe, index, stdout, marker))
            .collect();
        checks.extend(
            self.reads
                .iter()
                .enumerate()
                .map(|(index, probe)| read_check(probe, index, stdout)),
        );
        checks.extend(
            self.ports.iter().enumerate().map(|(index, (label, port))| {
                connect_check(label, *port, index, self.bash, stdout)
            }),
        );
        checks
    }
}

/// The write probes: the live pulse home, the live user home and the
/// working copy, and the live vendor dir when there is one.
fn write_probes(item: &ItemScratch, targets: &LiveTargets<'_>) -> Vec<WriteProbe> {
    let mut probes = vec![
        WriteProbe {
            label: "live pulse home".into(),
            dir: targets.live_root.to_path_buf(),
            expect_write: false,
        },
        WriteProbe {
            label: "live user home".into(),
            dir: targets.live_home.to_path_buf(),
            expect_write: false,
        },
        WriteProbe {
            label: "probe working copy".into(),
            dir: item.root().to_path_buf(),
            expect_write: true,
        },
    ];
    if let Some(vendor) = targets.vendor_home.filter(|v| v.is_dir()) {
        probes.push(WriteProbe {
            label: "live vendor dir".into(),
            dir: vendor.to_path_buf(),
            expect_write: false,
        });
    }
    probes
}

/// Run `script` in `/bin/sh` exactly as the item's CLI child runs: its
/// working copy, its env, its sandbox. The stdout, or the failed check.
async fn run_in_sandbox(item: &ItemScratch, script: String) -> Result<String, Check> {
    let mut cmd = tokio::process::Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(&script)
        .current_dir(item.root())
        .env_clear()
        .envs(std::env::vars_os().filter(|(k, _)| k == "PATH"))
        .envs(item.overrides().env);
    let sandbox = item.overrides().sandbox.unwrap_or_default();
    let prepared = sandbox
        .prepare()
        .map_err(|e| Check::new("sandbox", false, e.to_string()))?;
    let output = prepared
        .output(cmd)
        .await
        .map_err(|e| Check::new("sandbox", false, format!("spawn failed: {e}")))?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Did the shell print exactly `line`?
fn said(stdout: &str, line: &str) -> bool {
    stdout.lines().any(|l| l == line)
}

/// One write probe's check. A marker left behind is removed and counted as
/// a write.
fn write_check(probe: &WriteProbe, index: usize, stdout: &str, marker: &str) -> Check {
    let wrote = said(stdout, &format!("W {index}"));
    let denied = said(stdout, &format!("D {index}"));
    let left = probe.dir.join(marker);
    let leftover = left.exists();
    if leftover {
        let _ = std::fs::remove_file(&left);
    }
    let (passed, detail) = write_verdict(probe, wrote || leftover, denied);
    Check::new(format!("write: {}", probe.label), passed, detail)
}

fn write_verdict(probe: &WriteProbe, wrote: bool, denied: bool) -> (bool, String) {
    match (probe.expect_write, wrote, denied) {
        (false, false, true) => (true, "write refused by the kernel".to_string()),
        (true, true, _) => (true, "write allowed".to_string()),
        (false, true, _) => (false, format!("WROTE to {}", probe.dir.display())),
        (true, false, _) => (false, "the working copy is not writable".to_string()),
        (false, false, false) => (false, "the check did not run".to_string()),
    }
}

fn read_check(probe: &ReadProbe, index: usize, stdout: &str) -> Check {
    let read = said(stdout, &format!("R {index}"));
    let unreadable = said(stdout, &format!("U {index}"));
    let (passed, detail) = read_verdict(probe, read, unreadable);
    Check::new(format!("read: {}", probe.label), passed, detail)
}

fn read_verdict(probe: &ReadProbe, read: bool, unreadable: bool) -> (bool, String) {
    match (probe.expect_read, read, unreadable) {
        (false, false, true) => (true, "read refused by the kernel".to_string()),
        (true, true, _) => (true, "read allowed".to_string()),
        (false, true, _) => (false, format!("READ {}", probe.path.display())),
        (true, false, _) => (false, format!("{} is not readable", probe.path.display())),
        (_, false, false) => (false, "the check did not run".to_string()),
    }
}

/// One port's check: the connect must have been refused by the kernel.
/// EACCES from Landlock (a port it does not allow), EPERM from the unit's
/// address filter (127.0.0.0/8 denied).
fn connect_check(label: &str, port: u16, index: usize, bash: bool, stdout: &str) -> Check {
    if !bash {
        return Check::new(
            format!("connect: {label}"),
            false,
            "not checked: /bin/bash is missing",
        );
    }
    let prefix = format!("T{index} ");
    let tcp: Vec<&str> = stdout
        .lines()
        .filter_map(|l| l.strip_prefix(prefix.as_str()))
        .collect();
    let refused = tcp
        .iter()
        .any(|l| l.contains("Permission denied") || l.contains("Operation not permitted"));
    Check::new(
        format!("connect: 127.0.0.1:{port} ({label})"),
        refused,
        if refused {
            "connect refused by the kernel".to_string()
        } else {
            format!("not refused: {}", tcp.join(" "))
        },
    )
}

/// What the self-test shell expects when it reads one path.
struct ReadProbe {
    label: String,
    path: PathBuf,
    expect_read: bool,
}

/// Every deny-listed path that exists (a missing one has nothing to read),
/// and two that must stay readable: the item's own copy and a system file.
fn read_probes(item: &ItemScratch, deny: &DenyList) -> Vec<ReadProbe> {
    let mut probes: Vec<ReadProbe> = deny
        .paths
        .iter()
        .filter(|(_, path)| path.exists())
        .map(|(label, path)| ReadProbe {
            label: label.clone(),
            path: path.clone(),
            expect_read: false,
        })
        .collect();
    probes.push(ReadProbe {
        label: "probe working copy".into(),
        path: item.root().to_path_buf(),
        expect_read: true,
    });
    probes.push(ReadProbe {
        label: "system file /etc/hosts".into(),
        path: PathBuf::from("/etc/hosts"),
        expect_read: true,
    });
    probes
}

/// Local ports the child must not reach: the pulse, the live graph, and
/// what the unit blocks by address (nginx on 127.0.0.1:443).
fn connect_probes(targets: &LiveTargets<'_>) -> Vec<(&'static str, u16)> {
    let mut ports = vec![("the pulse", targets.pulse_port)];
    if let Some(port) = targets.graph_port {
        ports.push(("the live graph server", port));
    }
    ports.extend(
        targets
            .unit_ports
            .iter()
            .map(|port| ("the host on loopback, under the unit", *port)),
    );
    ports
}

/// Entries under `dir` (to a small depth) whose name contains `token` — a
/// probe that leaked into a live vendor directory leaves its scratch id in
/// the session or project directory name.
#[must_use]
pub fn entries_naming(dir: &Path, token: &str) -> Vec<PathBuf> {
    walkdir::WalkDir::new(dir)
        .max_depth(4)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().contains(token))
        .map(|e| e.path().to_path_buf())
        .collect()
}

/// Regular files under `dir`.
#[must_use]
pub fn file_count(dir: &Path) -> usize {
    walkdir::WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli_provider::adapters::claude::Claude;
    use crate::cli_provider::sandbox::landlock_or_skip;
    use crate::cli_provider::CliProvider;
    use crate::vigil::probe::scratch::tests::{plan, test_read_roots, Fixture};
    use crate::vigil::probe::scratch::{tree_digest, GraphTarget, ItemKind, ItemPlan};
    use pulse_system_types::llm::{Message, MessageContent, Role};

    fn write_probe(expect_write: bool) -> WriteProbe {
        WriteProbe {
            label: "live pulse home".into(),
            dir: PathBuf::from("/live/pulse"),
            expect_write,
        }
    }

    fn read_probe(expect_read: bool) -> ReadProbe {
        ReadProbe {
            label: "secrets".into(),
            path: PathBuf::from("/live/pulse/secrets"),
            expect_read,
        }
    }

    #[test]
    fn a_live_write_passes_only_when_the_kernel_refused_it() {
        let live = write_probe(false);
        assert_eq!(
            write_verdict(&live, false, true),
            (true, "write refused by the kernel".into())
        );
        assert_eq!(
            write_verdict(&live, true, false),
            (false, "WROTE to /live/pulse".into())
        );
        assert_eq!(
            write_verdict(&live, true, true),
            (false, "WROTE to /live/pulse".into()),
            "a write that happened fails, whatever else was printed"
        );
        assert_eq!(
            write_verdict(&live, false, false),
            (false, "the check did not run".into())
        );
        let copy = write_probe(true);
        assert_eq!(
            write_verdict(&copy, true, false),
            (true, "write allowed".into())
        );
        assert_eq!(
            write_verdict(&copy, false, true),
            (false, "the working copy is not writable".into())
        );
    }

    #[test]
    fn a_denied_read_passes_only_when_the_kernel_refused_it() {
        let denied = read_probe(false);
        assert_eq!(
            read_verdict(&denied, false, true),
            (true, "read refused by the kernel".into())
        );
        assert_eq!(
            read_verdict(&denied, true, false),
            (false, "READ /live/pulse/secrets".into())
        );
        assert_eq!(
            read_verdict(&denied, false, false),
            (false, "the check did not run".into())
        );
        let allowed = read_probe(true);
        assert_eq!(
            read_verdict(&allowed, true, false),
            (true, "read allowed".into())
        );
        assert_eq!(
            read_verdict(&allowed, false, true),
            (false, "/live/pulse/secrets is not readable".into())
        );
        assert_eq!(
            read_verdict(&allowed, false, false),
            (false, "/live/pulse/secrets is not readable".into()),
            "a path that must be readable and printed nothing is not readable"
        );
    }

    #[test]
    fn a_marker_left_behind_counts_as_a_write_and_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let probe = WriteProbe {
            label: "live user home".into(),
            dir: dir.path().to_path_buf(),
            expect_write: false,
        };
        std::fs::write(dir.path().join(".marker"), "").unwrap();
        let check = write_check(&probe, 0, "D 0\n", ".marker");
        assert!(!check.passed, "{check:?}");
        assert_eq!(check.name, "write: live user home");
        assert!(!dir.path().join(".marker").exists());
        assert!(write_check(&probe, 0, "D 0\n", ".marker").passed);
        assert!(
            !write_check(&probe, 1, "D 0\n", ".marker").passed,
            "line 1 never ran"
        );
    }

    #[test]
    fn a_connect_passes_only_when_the_kernel_refused_it() {
        let refused = "T0 /bin/bash: connect: Permission denied\nT1 bash: connect: Operation not permitted\nT2 \n";
        let landlock = connect_check("the pulse", 3200, 0, true, refused);
        assert_eq!(
            landlock,
            Check::new(
                "connect: 127.0.0.1:3200 (the pulse)",
                true,
                "connect refused by the kernel"
            )
        );
        assert!(connect_check("the live graph server", 8787, 1, true, refused).passed);
        let open = connect_check(
            "the host on loopback, under the unit",
            443,
            2,
            true,
            refused,
        );
        assert!(!open.passed);
        assert_eq!(open.detail, "not refused: ");
        let unchecked = connect_check("the pulse", 3200, 0, false, refused);
        assert_eq!(
            unchecked,
            Check::new(
                "connect: the pulse",
                false,
                "not checked: /bin/bash is missing"
            )
        );
    }

    #[test]
    fn the_script_tries_every_probe_and_connects_only_with_bash() {
        let mut test = SelfTest {
            writes: vec![write_probe(false)],
            reads: vec![read_probe(false), {
                let mut hosts = read_probe(true);
                hosts.path = PathBuf::from("/etc/hosts");
                hosts
            }],
            ports: vec![("the pulse", 3200)],
            bash: true,
        };
        let script = test.script(".m");
        let lines: Vec<&str> = script.lines().collect();
        assert_eq!(lines.len(), 4, "{script}");
        assert!(lines[0].contains("/live/pulse/.m") && lines[0].contains("echo 'W 0'"));
        assert!(lines[1].contains("head -c 1 ") && lines[1].contains("echo 'U 0'"));
        assert!(lines[2].contains("head -c 1 ") && lines[2].contains("/etc/hosts"));
        assert!(lines[2].contains("echo 'R 1'"));
        assert!(lines[3].contains("/dev/tcp/127.0.0.1/3200") && lines[3].contains("T0 "));
        test.bash = false;
        assert!(!test.script(".m").contains("/dev/tcp"));
        let checks = test.checks("U 0\nR 1\n", ".m");
        let names: Vec<&str> = checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "write: live pulse home",
                "read: secrets",
                "read: secrets",
                "connect: the pulse"
            ]
        );
        assert_eq!(
            checks.iter().map(|c| c.passed).collect::<Vec<_>>(),
            [false, true, true, false]
        );
    }

    #[tokio::test]
    async fn the_self_test_passes_inside_a_sandboxed_item() {
        if !landlock_or_skip("the_self_test_passes_inside_a_sandboxed_item") {
            return;
        }
        let fixture = Fixture::new();
        let vendor = fixture.home.path().join(".claude");
        std::fs::create_dir_all(&vendor).unwrap();
        let scratch = fixture.scratch(false);
        let graph = GraphTarget::Embedded;
        let item = scratch
            .item(0, ItemKind::Pulse, &plan(&Claude, &graph))
            .unwrap();
        let root = fixture.root();
        let credentials = tempfile::tempdir().unwrap();
        std::fs::write(credentials.path().join("root-password"), "fixture").unwrap();
        std::fs::create_dir_all(fixture.home.path().join(".ssh")).unwrap();
        let deny = DenyList::live(
            &root,
            fixture.home.path(),
            Some(credentials.path()),
            Some(&vendor),
        );
        let checks = sandbox_self_test(
            &item,
            &LiveTargets {
                live_root: &root,
                live_home: fixture.home.path(),
                vendor_home: Some(&vendor),
                pulse_port: 9,
                graph_port: Some(8),
                deny: &deny,
                unit_ports: &[],
            },
            ".marker-test",
        )
        .await;
        assert!(checks.iter().all(|c| c.passed), "{checks:#?}");
        let named = |prefix: &str| checks.iter().filter(|c| c.name.starts_with(prefix)).count();
        assert_eq!(named("write:"), 4, "{checks:#?}");
        assert_eq!(named("connect:"), 2, "{checks:#?}");
        // The live home, its secrets/ and record/, ~/.ssh, the user's home,
        // the credential and vendor dirs (~/.config is absent, so not
        // probed), plus the two readable controls.
        assert_eq!(named("read:"), 9, "{checks:#?}");
        assert!(checks
            .iter()
            .any(|c| c.name == "read: graph credential dir" && c.passed));
    }

    /// The CI isolation test: a fake CLI run as a probe item's child tries
    /// to write into the live pulse home, the user's home and the vendor
    /// dir. The kernel refuses every one, the live trees are unchanged, and
    /// the reply and tool calls still come back.
    #[tokio::test]
    async fn a_probe_child_cannot_write_live_and_its_turn_is_captured() {
        if !landlock_or_skip("a_probe_child_cannot_write_live_and_its_turn_is_captured") {
            return;
        }
        let fixture = Fixture::new();
        let root = fixture.root();
        let home = fixture.home.path().to_path_buf();
        let vendor = home.join(".claude");
        std::fs::create_dir_all(vendor.join("projects")).unwrap();
        std::fs::write(vendor.join("settings.json"), "{}").unwrap();

        let bin_dir = tempfile::tempdir().unwrap();
        let bin = bin_dir.path().join("fake-claude");
        let attempts: String = [&root, &home, &vendor, &vendor.join("projects")]
            .iter()
            .map(|dir| {
                let path = shell_quote(&dir.join("leak"));
                format!("( : > {path} ) 2>>\"$PWD/denied.log\"\n")
            })
            .collect();
        let peeks: String = [root.join("SELF.md"), root.join("secrets/graph-password")]
            .iter()
            .map(|file| {
                let path = shell_quote(file);
                format!("cat {path} >> \"$PWD/peeked\" 2>>\"$PWD/denied.log\"\n")
            })
            .collect();
        let script = format!(
            "#!/bin/sh\ncat > /dev/null\n{attempts}{peeks}echo here > \"$PWD/inside\"\n\
             printf '%s\\n' \
             '{{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"m-configured\"}}' \
             '{{\"type\":\"assistant\",\"message\":{{\"model\":\"m-served\",\"content\":[{{\"type\":\"tool_use\",\"name\":\"Read\",\"input\":{{\"file_path\":\"record/probes/runs.jsonl\"}}}}]}}}}' \
             '{{\"type\":\"result\",\"result\":\"probe ok\",\"is_error\":false,\"usage\":{{\"input_tokens\":5,\"output_tokens\":2}}}}'\n"
        );
        std::fs::write(&bin, script).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let before = (tree_digest(&root, &[]), tree_digest(&home, &[]));
        let scratch = fixture.scratch(false);
        let graph = GraphTarget::Embedded;
        let mut read_roots = test_read_roots().to_vec();
        read_roots.push(bin_dir.path().to_path_buf());
        let item = scratch
            .item(
                0,
                ItemKind::Pulse,
                &ItemPlan {
                    read_roots: &read_roots,
                    ..plan(&Claude, &graph)
                },
            )
            .unwrap();
        let mut overrides = item.overrides();
        overrides
            .env
            .push(("HOME".into(), home.as_os_str().to_owned()));
        let provider = CliProvider::new(
            Box::new(Claude),
            Some(bin.display().to_string()),
            "m-configured".into(),
            item.root().to_path_buf(),
        )
        .with_overrides(overrides);
        let run = provider
            .invoke_captured(
                "system",
                &[Message {
                    role: Role::User,
                    content: MessageContent::Text("say ok".into()),
                    source: None,
                }],
            )
            .await
            .expect("captured run");

        assert_eq!(run.reply, "probe ok");
        assert_eq!(run.served_model.as_deref(), Some("m-served"));
        assert_eq!(run.tool_calls.len(), 1);
        assert_eq!(run.tool_calls[0].name, "Read");
        assert_eq!(run.usage.output_tokens, Some(2));
        assert!(item.root().join("inside").exists(), "the copy is writable");
        let denied = std::fs::read_to_string(item.root().join("denied.log")).unwrap();
        // The capability probe runs the same binary, sandboxed the same way.
        assert!(denied.matches("Permission denied").count() >= 6, "{denied}");
        let peeked = std::fs::read_to_string(item.root().join("peeked")).unwrap_or_default();
        assert!(peeked.is_empty(), "live files are unreadable: {peeked}");
        for dir in [&root, &home, &vendor] {
            assert!(!dir.join("leak").exists(), "{}", dir.display());
        }
        assert_eq!(
            (tree_digest(&root, &[]), tree_digest(&home, &[])),
            before,
            "the live pulse home and the user's home are unchanged"
        );
        assert!(entries_naming(&vendor, scratch.id()).is_empty());
        item.finish();
    }

    #[test]
    fn entries_naming_finds_a_leaked_scratch_id() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("projects/-x-s-abc123-items-0")).unwrap();
        assert_eq!(entries_naming(dir.path(), "abc123").len(), 1);
        assert!(entries_naming(dir.path(), "zzz").is_empty());
        std::fs::write(dir.path().join("projects/f"), "x").unwrap();
        assert_eq!(file_count(dir.path()), 1);
    }

    /// By hand only, never in the gate: does a real agent CLI start under
    /// the read fence? Runs `<bin> --version` (no model call, no login) in a
    /// fixture item with the production read fence:
    /// `PULSE_NULL_PROBE_REAL_CLI=/usr/local/bin/claude PULSE_NULL_PROBE_REAL_ADAPTER=claude
    /// cargo test a_real_cli_starts -- --ignored`.
    #[tokio::test]
    #[ignore = "runs a real agent CLI's --version (see doc comment)"]
    async fn a_real_cli_starts_under_the_read_fence() {
        let (Ok(bin), Ok(adapter)) = (
            std::env::var("PULSE_NULL_PROBE_REAL_CLI"),
            std::env::var("PULSE_NULL_PROBE_REAL_ADAPTER"),
        ) else {
            panic!("set PULSE_NULL_PROBE_REAL_CLI and _ADAPTER: this test checks that CLI");
        };
        if !landlock_or_skip("a_real_cli_starts_under_the_read_fence") {
            return;
        }
        let adapter = crate::cli_provider::adapters::by_name(&adapter).unwrap();
        let fixture = Fixture::new();
        let root = fixture.root();
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap();
        let deny = DenyList::live(&root, &home, None, adapter.vendor_home(&home).as_deref());
        let read_roots =
            crate::vigil::probe::fence::read_roots(adapter.as_ref(), &bin, &home, &deny).unwrap();
        let scratch = fixture.scratch(false);
        let graph = GraphTarget::Embedded;
        let item = scratch
            .item(
                0,
                ItemKind::Bare,
                &ItemPlan {
                    read_roots: &read_roots,
                    ..plan(adapter.as_ref(), &graph)
                },
            )
            .unwrap();
        let provider = CliProvider::new(
            adapter,
            Some(bin.clone()),
            String::new(),
            item.root().to_path_buf(),
        )
        .with_overrides(item.overrides());
        let version = provider.cli_version().await;
        eprintln!("{bin} --version under the read fence: {version:?}");
        assert!(version.is_ok(), "{version:?}");
        item.finish();
    }
}
