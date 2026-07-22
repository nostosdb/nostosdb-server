use std::fs;
use std::path::PathBuf;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn native_services_execute_the_same_daemon_config_contract() {
    let systemd = fs::read_to_string(root().join("distribution/systemd/nostosdb.service"))
        .expect("systemd candidate reads");
    assert!(
        systemd
            .contains("ExecStart=/usr/local/bin/nostosd serve --config /etc/nostosdb/server.toml")
    );
    assert!(systemd.contains("User=nostosdb"));
    assert!(systemd.contains("ReadWritePaths=/var/lib/nostosdb"));

    let homebrew = fs::read_to_string(root().join("distribution/homebrew/Formula/nostosdb.rb.in"))
        .expect("Homebrew candidate reads");
    assert!(homebrew.contains("class Nostosdb < Formula"));
    assert!(homebrew.contains("bin.install \"nostos\", \"nostosd\""));
    assert!(homebrew.contains(
        "run [opt_bin/\"nostosd\", \"serve\", \"--config\", (nostos_home/\"config/server.toml\").to_s]"
    ));
    assert!(homebrew.contains("Pathname.new(Dir.home)/\".nostosdb\""));
    assert!(homebrew.contains("data_dir = nostos_home/\"data\""));
    assert!(homebrew.contains("logs_dir = nostos_home/\"logs\""));
    assert!(homebrew.contains("environment_variables NOSTOS_HOME: nostos_home.to_s"));
    assert!(!homebrew.contains("etc/\"nostosdb"));
    assert!(!homebrew.contains("var/\"nostosdb"));
    assert!(homebrew.contains("\"--listen\", \"127.0.0.1:7878\""));

    let windows = fs::read_to_string(root().join("distribution/windows/install-service.ps1"))
        .expect("Windows Service candidate reads");
    assert!(windows.contains("nostosd.exe"));
    assert!(windows.contains("serve --config"));
    assert!(windows.contains("$env:ProgramData\\NostosDB\\server.toml"));
    assert!(!windows.contains("NOSTOS_CREDENTIAL="));
}

#[test]
fn docker_uses_persistent_config_and_data_volumes_without_publication() {
    let dockerfile = fs::read_to_string(root().join("Dockerfile")).expect("Dockerfile reads");
    assert!(dockerfile.contains("ENTRYPOINT [\"nostosd\"]"));
    assert!(dockerfile.contains("CMD [\"serve\", \"--config\", \"/etc/nostosdb/server.toml\"]"));
    assert!(dockerfile.contains("/var/lib/nostosdb"));
    assert!(!dockerfile.contains("docker push"));

    let compose = fs::read_to_string(root().join("compose.yaml")).expect("compose reads");
    assert!(compose.contains("nostos-config:/etc/nostosdb"));
    assert!(compose.contains("nostos-data:/var/lib/nostosdb"));
    assert!(compose.contains("127.0.0.1:7878:7878"));
    assert!(!compose.contains("latest"));
}
