//! The receipt smoke script, against this package's binary.

#[test]
fn receipt_install_smoke_script_exits_0() {
    let bin = std::env::var("CARGO_BIN_EXE_pulse_null").unwrap_or_else(|_| {
        let exe = std::env::current_exe().expect("test executable");
        // target/debug/deps/<test> -> target/debug/pulse-null
        exe.parent()
            .and_then(|deps| deps.parent())
            .map(|debug| debug.join("pulse-null"))
            .expect("debug dir")
            .display()
            .to_string()
    });
    let manifest = env!("CARGO_MANIFEST_DIR");
    let output = std::process::Command::new("bash")
        .arg(format!("{manifest}/scripts/smoke-receipts.sh"))
        .env("PULSE_NULL", &bin)
        .current_dir(manifest)
        .output()
        .expect("smoke script");
    assert!(
        output.status.success(),
        "smoke script exited {} with binary {bin}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
