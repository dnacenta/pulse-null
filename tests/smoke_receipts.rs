//! The receipt smoke script, against this package's binary.

#[test]
fn receipt_install_smoke_script_exits_0() {
    let bin = env!("CARGO_BIN_EXE_pulse-null");
    let manifest = env!("CARGO_MANIFEST_DIR");
    let mut child = std::process::Command::new("bash")
        .arg(format!("{manifest}/scripts/smoke-receipts.sh"))
        .env("PULSE_NULL", bin)
        .current_dir(manifest)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("smoke script");
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let stdout_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stdout, &mut buf);
        buf
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stderr, &mut buf);
        buf
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("smoke script exceeded 300s");
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
            Err(err) => panic!("smoke script: {err}"),
        }
    };
    let stdout = String::from_utf8_lossy(&stdout_thread.join().expect("stdout")).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_thread.join().expect("stderr")).into_owned();
    assert!(
        status.success(),
        "smoke script exited {status} with binary {bin}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
