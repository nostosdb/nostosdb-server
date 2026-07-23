#![forbid(unsafe_code)]

use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use nostdb_server::config::DaemonConfig;
use nostdb_server::daemon::DatabaseDaemon;
use tracing_subscriber::EnvFilter;

const HELP: &str = "NostDB installable database daemon

Usage:
    nostd init --data-dir PATH [--config PATH] [--listen IP:PORT]
    nostd serve [--config PATH]
    nostd --help
    nostd --version

Initialization creates separate protected client and admin credential files.
Credential values are never accepted as command-line arguments.
Configuration lookup is --config, NOSTDB_CONFIG, NOSTDB_HOME/config/server.toml,
then the platform default.
The default database-protocol listener is 127.0.0.1:7878.";

const INIT_HELP: &str = "Initialize a fresh NostDB daemon installation

Usage:
    nostd init --data-dir PATH [--config PATH] [--listen IP:PORT]

Options:
    --data-dir PATH   Required fresh or empty daemon-owned data directory
    --config PATH     New configuration file; defaults to the platform lookup path
    --listen IP:PORT  Canonical numeric listener address [default: 127.0.0.1:7878]
    -h, --help        Show this help

Initialization never adopts existing data and never replaces an existing config.";

const SERVE_HELP: &str = "Run the NostDB database daemon in the foreground

Usage:
    nostd serve [--config PATH]

Options:
    --config PATH  Existing configuration file; defaults to the platform lookup path
    -h, --help     Show this help

SIGINT and, on Unix, SIGTERM request a graceful shutdown.";

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("nostd: {error}");
            ExitCode::from(2)
        }
    }
}

async fn run() -> Result<(), String> {
    let mut arguments = env::args().skip(1).collect::<Vec<_>>();
    if arguments.is_empty() || matches!(arguments[0].as_str(), "-h" | "--help") {
        println!("{HELP}");
        return Ok(());
    }
    if matches!(arguments[0].as_str(), "-V" | "--version") {
        if arguments.len() != 1 {
            return Err("--version does not accept arguments".to_owned());
        }
        println!("nostd {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let command = arguments.remove(0);
    match command.as_str() {
        "init" if help_requested(&arguments) => {
            println!("{INIT_HELP}");
            Ok(())
        }
        "init" => initialize(parse_init(arguments)?),
        "serve" if help_requested(&arguments) => {
            println!("{SERVE_HELP}");
            Ok(())
        }
        "serve" => serve(parse_serve(arguments)?).await,
        _ => Err(format!("unknown command `{command}`\n\n{HELP}")),
    }
}

fn help_requested(arguments: &[String]) -> bool {
    arguments
        .iter()
        .any(|argument| matches!(argument.as_str(), "-h" | "--help"))
}

struct InitOptions {
    data_directory: PathBuf,
    config_path: PathBuf,
    listen: String,
}

fn parse_init(arguments: Vec<String>) -> Result<InitOptions, String> {
    let mut data_directory = None;
    let mut config_path = None;
    let mut listen = "127.0.0.1:7878".to_owned();
    let mut index = 0;
    while index < arguments.len() {
        let option = &arguments[index];
        match option.as_str() {
            "--data-dir" => data_directory = Some(value(&arguments, &mut index)?.into()),
            "--config" => config_path = Some(value(&arguments, &mut index)?.into()),
            "--listen" => listen = value(&arguments, &mut index)?.to_owned(),
            _ => return Err(format!("unknown init option `{option}`")),
        }
        index += 1;
    }
    let data_directory =
        data_directory.ok_or_else(|| "init requires --data-dir PATH".to_owned())?;
    let config_path = config_path.unwrap_or_else(default_config_path);
    Ok(InitOptions {
        data_directory,
        config_path,
        listen,
    })
}

fn parse_serve(arguments: Vec<String>) -> Result<PathBuf, String> {
    let mut config_path = None;
    let mut index = 0;
    while index < arguments.len() {
        let option = &arguments[index];
        match option.as_str() {
            "--config" => config_path = Some(value(&arguments, &mut index)?.into()),
            _ => return Err(format!("unknown serve option `{option}`")),
        }
        index += 1;
    }
    Ok(config_path.unwrap_or_else(default_config_path))
}

fn value<'a>(arguments: &'a [String], index: &mut usize) -> Result<&'a str, String> {
    *index += 1;
    arguments
        .get(*index)
        .map(String::as_str)
        .ok_or_else(|| "option requires a value".to_owned())
}

fn initialize(options: InitOptions) -> Result<(), String> {
    let report = DatabaseDaemon::initialize(
        &options.config_path,
        &options.data_directory,
        &options.listen,
    )
    .map_err(|error| error.to_string())?;
    println!("initialized NostDB data directory");
    println!("config: {}", report.config_path.display());
    println!("data: {}", report.data_directory.display());
    println!(
        "client credential: {}",
        report.query_credential_file.display()
    );
    println!(
        "admin credential: {}",
        report.admin_credential_file.display()
    );
    Ok(())
}

async fn serve(config_path: PathBuf) -> Result<(), String> {
    let config = DaemonConfig::load(&config_path).map_err(|error| error.to_string())?;
    let listen = config.listen_address().map_err(|error| error.to_string())?;
    init_tracing()?;
    let daemon = DatabaseDaemon::open(config).map_err(|error| error.to_string())?;
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|error| format!("cannot bind database protocol listener {listen}: {error}"))?;
    tracing::info!(address = %listen, data_directory = %daemon.config().data_directory.display(), "NostDB database daemon listening");
    nostdb_server::serve_database_protocol(listener, daemon, shutdown_signal())
        .await
        .map_err(|error| error.to_string())
}

fn default_config_path() -> PathBuf {
    select_default_config_path(
        environment_path("NOSTDB_CONFIG"),
        environment_path("NOSTDB_HOME"),
        platform_default_config_path(),
    )
}

fn select_default_config_path(
    config_path: Option<PathBuf>,
    nostdb_home: Option<PathBuf>,
    platform_default: PathBuf,
) -> PathBuf {
    config_path
        .or_else(|| nostdb_home.map(|path| path.join("config/server.toml")))
        .unwrap_or(platform_default)
}

fn environment_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn platform_default_config_path() -> PathBuf {
    #[cfg(windows)]
    if let Some(program_data) = environment_path("PROGRAMDATA") {
        return program_data.join("NostDB/server.toml");
    }
    #[cfg(target_os = "macos")]
    if let Some(home) = environment_path("HOME") {
        return home.join(".nostdb/config/server.toml");
    }
    #[cfg(target_os = "linux")]
    {
        return PathBuf::from("/etc/nostdb/server.toml");
    }
    #[allow(unreachable_code)]
    env::current_dir()
        .unwrap_or_else(|_| Path::new(".").to_path_buf())
        .join("server.toml")
}

fn init_tracing() -> Result<(), String> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    if env::var("NOSTDB_LOG_FORMAT").as_deref() == Ok("json") {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .try_init()
            .map_err(|error| error.to_string())
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .try_init()
            .map_err(|error| error.to_string())
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!(%error, "failed to install shutdown signal handler");
    }
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(terminate) => terminate,
        Err(error) => {
            tracing::error!(%error, "failed to install SIGTERM handler");
            if let Err(error) = tokio::signal::ctrl_c().await {
                tracing::error!(%error, "failed to install shutdown signal handler");
            }
            return;
        }
    };
    tokio::select! {
        result = tokio::signal::ctrl_c() => {
            if let Err(error) = result {
                tracing::error!(%error, "failed to install shutdown signal handler");
            }
        }
        signal = terminate.recv() => {
            if signal.is_none() {
                tracing::error!("SIGTERM signal stream ended unexpectedly");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::select_default_config_path;
    use std::path::PathBuf;

    #[test]
    fn config_environment_has_priority_over_nostdb_home() {
        assert_eq!(
            select_default_config_path(
                Some(PathBuf::from("/explicit/server.toml")),
                Some(PathBuf::from("/user/.nostdb")),
                PathBuf::from("/platform/server.toml"),
            ),
            PathBuf::from("/explicit/server.toml")
        );
    }

    #[test]
    fn nostdb_home_contains_the_default_config_directory() {
        assert_eq!(
            select_default_config_path(
                None,
                Some(PathBuf::from("/user/.nostdb")),
                PathBuf::from("/platform/server.toml"),
            ),
            PathBuf::from("/user/.nostdb/config/server.toml")
        );
    }
}
