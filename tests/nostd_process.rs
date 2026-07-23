use std::fs;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_nostd")
}

fn test_root(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nostd-process-{name}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ))
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("child status reads") {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn subcommand_help_is_successful_and_actionable() {
    for (command, expected_option) in [("init", "--data-dir"), ("serve", "--config")] {
        let output = Command::new(binary())
            .args([command, "--help"])
            .output()
            .expect("help process starts");
        assert!(output.status.success(), "{command} help must succeed");
        assert!(
            output.stderr.is_empty(),
            "{command} help stderr must be empty"
        );
        let stdout = String::from_utf8(output.stdout).expect("help is UTF-8");
        assert!(stdout.contains(&format!("nostd {command}")));
        assert!(stdout.contains(expected_option));
    }
}

#[cfg(unix)]
#[test]
fn sigterm_uses_the_graceful_shutdown_path() {
    let root = test_root("sigterm");
    let config_path = root.join("server.toml");
    let data_directory = root.join("data");
    fs::create_dir(&root).expect("test directory creates");
    let reserved = TcpListener::bind("127.0.0.1:0").expect("ephemeral listener binds");
    let listen = reserved.local_addr().expect("ephemeral address reads");
    drop(reserved);

    let initialized = Command::new(binary())
        .args([
            "init",
            "--data-dir",
            data_directory.to_str().expect("data path is UTF-8"),
            "--config",
            config_path.to_str().expect("config path is UTF-8"),
            "--listen",
            &listen.to_string(),
        ])
        .output()
        .expect("init process starts");
    assert!(
        initialized.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&initialized.stderr)
    );

    let mut daemon = Command::new(binary())
        .args([
            "serve",
            "--config",
            config_path.to_str().expect("config path is UTF-8"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("daemon process starts");
    let query_credential = fs::read_to_string(data_directory.join("credentials/client.token"))
        .expect("query credential reads");
    let deadline = Instant::now() + Duration::from_secs(5);
    let _active_client = loop {
        match nostdb_client::Client::connect(
            &format!("nostdb://{listen}"),
            query_credential.trim(),
            "SIGTERM process test",
        ) {
            Ok(client) => break client,
            Err(_) => {
                assert!(Instant::now() < deadline, "daemon did not start listening");
                thread::sleep(Duration::from_millis(20));
            }
        }
    };

    let signal_status = Command::new("kill")
        .args(["-TERM", &daemon.id().to_string()])
        .status()
        .expect("SIGTERM command starts");
    assert!(signal_status.success(), "SIGTERM command failed");
    let Some(status) = wait_for_exit(&mut daemon, Duration::from_secs(5)) else {
        daemon.kill().expect("timed-out daemon kills");
        daemon.wait().expect("killed daemon reaps");
        panic!("daemon did not finish graceful SIGTERM shutdown");
    };
    assert!(status.success(), "SIGTERM exit was {status}");

    let config = nostdb_server::config::DaemonConfig::load(&config_path)
        .expect("configuration remains readable");
    drop(
        nostdb_server::daemon::DatabaseDaemon::open(config)
            .expect("graceful shutdown released daemon ownership"),
    );
    fs::remove_dir_all(root).expect("test directory removes");
}

#[cfg(unix)]
#[test]
fn serve_rejects_group_or_other_credential_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let root = test_root("credential-permissions");
    let config_path = root.join("server.toml");
    let data_directory = root.join("data");
    fs::create_dir(&root).expect("test directory creates");
    let initialized = Command::new(binary())
        .args([
            "init",
            "--data-dir",
            data_directory.to_str().expect("data path is UTF-8"),
            "--config",
            config_path.to_str().expect("config path is UTF-8"),
            "--listen",
            "127.0.0.1:0",
        ])
        .output()
        .expect("init process starts");
    assert!(
        initialized.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&initialized.stderr)
    );
    let credential_path = data_directory.join("credentials/client.token");
    fs::set_permissions(&credential_path, fs::Permissions::from_mode(0o644))
        .expect("credential permissions change");

    let served = Command::new(binary())
        .args([
            "serve",
            "--config",
            config_path.to_str().expect("config path is UTF-8"),
        ])
        .output()
        .expect("serve process starts");
    assert_eq!(served.status.code(), Some(2));
    let stderr = String::from_utf8(served.stderr).expect("serve diagnostic is UTF-8");
    assert!(stderr.contains("client.token"));
    assert!(stderr.contains("mode 644"));
    assert!(stderr.contains("0600 or stricter"));
    fs::remove_dir_all(root).expect("test directory removes");
}
