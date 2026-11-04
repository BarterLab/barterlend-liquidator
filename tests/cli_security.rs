use std::process::Command;

fn log_event(text: &str, level: &str) -> serde_json::Value {
    assert_eq!(text.lines().count(),1,"{text}");
    assert!(!text.contains('\u{1b}'),"ANSI escapes in container log");
    let (envelope,details)=text.trim_end().split_once(" details=").expect("text envelope and JSON event details");
    let fields:Vec<_>=envelope.split_whitespace().collect();
    assert!(fields[0].ends_with('Z') && fields[0].as_bytes().get(10)==Some(&b'T'));
    assert_eq!(fields[1],level);assert_eq!(fields[2],"barterlend_liquidator::diagnostics_io:");
    serde_json::from_str(details).unwrap()
}

fn write_manifest(directory: &tempfile::TempDir, testnet: bool) -> std::path::PathBuf {
    let mut manifest:serde_json::Value=serde_json::from_str(include_str!("../examples/local-manifest.json")).unwrap();
    if testnet {
        manifest["profile"]=serde_json::json!("monad-testnet");manifest["rpc_url"]=serde_json::json!("https://testnet-rpc.monad.xyz");
        manifest["chain"]["chain_id"]=serde_json::json!(10143);manifest["v3"]["chain"]["chain_id"]=serde_json::json!(10143);
    }
    let path=directory.path().join("manifest.json");std::fs::write(&path,serde_json::to_vec(&manifest).unwrap()).unwrap();path
}

#[test]
fn testnet_requires_file_never_uses_env_and_dry_run_does_not_open_key_file() {
    let directory=tempfile::tempdir().unwrap();let path=write_manifest(&directory,true);
    let sentinel="PRIVATE_KEY_SENTINEL_TESTNET_MUST_NOT_READ_ENV";
    for flags in [vec!["--testnet-live"], vec!["--local-live"], vec!["--private-key-file","/nonexistent-and-never-read"]] {
        let output=Command::new(env!("CARGO_BIN_EXE_barterlend-liquidator")).arg("--manifest").arg(&path)
            .args(flags).env("BL_LIQUIDATOR_PRIVATE_KEY",sentinel).output().unwrap();
        assert_eq!(output.status.code(),Some(1));assert!(output.stderr.is_empty());
        let text=String::from_utf8(output.stdout).unwrap();assert!(!text.contains(sentinel));
        let event=log_event(&text,"ERROR");
        assert_eq!(event["event"],"diagnostic_error");
    }
}

#[test]
fn malformed_testnet_key_file_is_redacted_before_network() {
    #[cfg(unix)] use std::os::unix::fs::PermissionsExt;
    let directory=tempfile::tempdir().unwrap();let path=write_manifest(&directory,true);
    let key=directory.path().join("key");let sentinel="PRIVATE_KEY_FILE_SENTINEL_DO_NOT_LOG";
    std::fs::write(&key,sentinel).unwrap();
    #[cfg(unix)] std::fs::set_permissions(&key,std::fs::Permissions::from_mode(0o600)).unwrap();
    let output=Command::new(env!("CARGO_BIN_EXE_barterlend-liquidator")).arg("--manifest").arg(path)
        .arg("--testnet-live").arg("--private-key-file").arg(key).output().unwrap();
    assert_eq!(output.status.code(),Some(1));assert!(output.stderr.is_empty());
    let text=String::from_utf8(output.stdout).unwrap();assert!(!text.contains(sentinel));
    assert_eq!(text.lines().count(),1);
}

#[cfg(unix)]
#[path = "support/closed_output.rs"]
mod closed_output;

#[test]
fn invalid_operator_key_is_never_printed_and_failure_is_one_text_event() {
    let directory = tempfile::tempdir().unwrap();
    let manifest: serde_json::Value = serde_json::from_str(include_str!("../examples/local-manifest.json")).unwrap();
    let path = directory.path().join("manifest.json");
    std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let sentinel = "PRIVATE_KEY_SENTINEL_DO_NOT_LOG_123456";
    let output = Command::new(env!("CARGO_BIN_EXE_barterlend-liquidator"))
        .arg("--manifest").arg(&path).arg("--local-live")
        .env("BL_LIQUIDATOR_PRIVATE_KEY", sentinel).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stderr.is_empty());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(!text.contains(sentinel));
    assert_eq!(text.lines().count(), 1);
    let event=log_event(&text,"ERROR");
    assert_eq!(event["event"], "diagnostic_error");
    assert_eq!(event["code"], "command_failed");
}

#[cfg(unix)]
#[test]
fn closed_error_stream_preserves_expected_cli_failure_without_panic() {
    let directory = tempfile::tempdir().unwrap();
    let manifest: serde_json::Value = serde_json::from_str(include_str!("../examples/local-manifest.json")).unwrap();
    let path = directory.path().join("manifest.json");
    std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_barterlend-liquidator"));
    command.arg("--manifest").arg(&path).arg("--local-live")
        .env("BL_LIQUIDATOR_PRIVATE_KEY", "invalid_key_for_closed_stream_test");
    closed_output::disconnect_output_peers(&mut command);
    assert_eq!(command.status().unwrap().code(), Some(1), "closed stderr must not turn a handled error into a panic");
}

#[cfg(unix)]
#[test]
fn fifo_manifest_is_rejected_without_blocking_or_network() {
    let directory = tempfile::tempdir().unwrap();
    let fifo = directory.path().join("manifest.fifo");
    assert!(Command::new("mkfifo").arg(&fifo).status().unwrap().success());
    let mut child = Command::new(env!("CARGO_BIN_EXE_barterlend-liquidator"))
        .arg("--manifest").arg(fifo).arg("--cycles").arg("1")
        .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        if child.try_wait().unwrap().is_some() { break; }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill(); let _ = child.wait(); panic!("CLI blocked opening a FIFO manifest");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(),Some(1));assert!(output.stderr.is_empty());
    let event=log_event(std::str::from_utf8(&output.stdout).unwrap(),"ERROR");
    assert_eq!(event["event"],"diagnostic_error");
}

#[test]
fn config_check_uses_text_stdout_and_respects_log_level() {
    let directory=tempfile::tempdir().unwrap();let path=write_manifest(&directory,false);
    for level in ["info","warn"] {
        let output=Command::new(env!("CARGO_BIN_EXE_barterlend-liquidator")).arg("--config").arg(&path)
            .args(["config","check"]).env("LOG_FILTER",level).env("RUST_LOG","trace")
            .env_remove("BL_PRIVATE_KEY_FILE").output().unwrap();
        assert!(output.status.success());assert!(output.stderr.is_empty());
        if level=="info" {
            let event=log_event(std::str::from_utf8(&output.stdout).unwrap(),"INFO");
            assert_eq!(event["event"],"config_valid");assert_eq!(event["network"],"local");
        } else {assert!(output.stdout.is_empty(),"INFO bypassed LOG_FILTER=warn");}
    }
}

#[test]
fn config_init_reports_missing_non_mainnet_quoter_before_rpc() {
    let dir=tempfile::tempdir().unwrap();let path=dir.path().join("config.toml");
    let output=Command::new(env!("CARGO_BIN_EXE_barterlend-liquidator"))
        .args(["--rpc-url","http://127.0.0.1:9","config","init","--network","local",
            "--executor","0x1111111111111111111111111111111111111111",
            "--operator","0x2222222222222222222222222222222222222222","--output"])
        .arg(&path).env("LOG_FILTER","info").env_remove("BL_PRIVATE_KEY_FILE").output().unwrap();
    assert_eq!(output.status.code(),Some(1));assert!(output.stderr.is_empty());assert!(!path.exists());
    let event=log_event(std::str::from_utf8(&output.stdout).unwrap(),"ERROR");
    assert_eq!(event["cause"],"config_quoter_required");
}
