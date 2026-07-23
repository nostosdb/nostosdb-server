use std::fs;
use std::path::PathBuf;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn native_services_execute_the_same_daemon_config_contract() {
    let systemd = fs::read_to_string(root().join("distribution/systemd/nostdb.service"))
        .expect("systemd candidate reads");
    assert!(
        systemd.contains("ExecStart=/usr/local/bin/nostd serve --config /etc/nostdb/server.toml")
    );
    assert!(systemd.contains("User=nostdb"));
    assert!(systemd.contains("ReadWritePaths=/var/lib/nostdb"));
    assert!(systemd.contains("StateDirectoryMode=0700"));
    assert!(!systemd.contains("ConfigurationDirectory="));
    let systemd_instructions = fs::read_to_string(root().join("distribution/systemd/README.md"))
        .expect("systemd initialization instructions read");
    assert!(systemd_instructions.contains("sudo --user=nostdb --"));
    assert!(systemd_instructions.contains("chown root:nostdb /etc/nostdb/server.toml"));
    assert!(systemd_instructions.contains("chown root:nostdb /etc/nostdb\n"));
    assert!(systemd_instructions.contains("chmod 0750 /etc/nostdb"));
    assert!(systemd_instructions.contains("0600"));

    let homebrew = fs::read_to_string(root().join("distribution/homebrew/Formula/nostdb.rb.in"))
        .expect("Homebrew candidate reads");
    assert!(homebrew.contains("class Nostdb < Formula"));
    assert!(homebrew.contains("bin.install \"nostdb\", \"nostd\""));
    assert!(homebrew.contains(
        "run [opt_bin/\"nostd\", \"serve\", \"--config\", (nostdb_home/\"config/server.toml\").to_s]"
    ));
    assert!(homebrew.contains("$HOME/.nostdb"));
    assert!(homebrew.contains("Homebrew runs post_install with a"));
    assert!(!homebrew.contains("def post_install"));
    assert!(homebrew.contains("--data-dir \"$HOME/.nostdb/data\""));
    assert!(homebrew.contains(
        "mkdir -p \"$HOME/.nostdb/data\" \"$HOME/.nostdb/config\" \"$HOME/.nostdb/logs\""
    ));
    assert!(homebrew.contains(
        "chmod 700 \"$HOME/.nostdb\" \"$HOME/.nostdb/data\" \"$HOME/.nostdb/config\" \"$HOME/.nostdb/logs\""
    ));
    assert!(homebrew.contains("environment_variables NOSTDB_HOME: nostdb_home.to_s"));
    assert!(!homebrew.contains("etc/\"nostdb"));
    assert!(!homebrew.contains("var/\"nostdb"));
    assert!(homebrew.contains("--listen 127.0.0.1:7878"));

    let windows = fs::read_to_string(root().join("distribution/windows/install-service.ps1"))
        .expect("Windows Service candidate reads");
    assert!(windows.contains("Windows Service registration is not implemented"));
    assert!(windows.contains("foreground console process"));
    assert!(!windows.contains("sc.exe create"));
    assert!(!windows.contains("NOSTDB_CREDENTIAL="));
}

#[test]
fn docker_uses_persistent_config_and_data_volumes_without_publication() {
    let dockerfile = fs::read_to_string(root().join("Dockerfile")).expect("Dockerfile reads");
    assert!(dockerfile.contains("ENTRYPOINT [\"nostd\"]"));
    assert!(dockerfile.contains("CMD [\"serve\", \"--config\", \"/etc/nostdb/server.toml\"]"));
    assert!(dockerfile.contains("/var/lib/nostdb"));
    assert!(!dockerfile.contains("docker push"));

    let compose = fs::read_to_string(root().join("compose.yaml")).expect("compose reads");
    assert!(compose.contains("nostdb-config:/etc/nostdb"));
    assert!(compose.contains("nostdb-data:/var/lib/nostdb"));
    assert!(compose.contains("127.0.0.1:7878:7878"));
    assert!(!compose.contains("latest"));

    let readme = fs::read_to_string(root().join("README.md")).expect("Server README reads");
    assert!(readme.contains("docker compose --profile init run --rm init"));
    assert!(readme.contains("docker compose up server"));
}
