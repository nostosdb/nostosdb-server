#![forbid(unsafe_code)]

use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use nostdb_server::{AppState, ServerConfig, router};
use tracing_subscriber::EnvFilter;

const HELP: &str = "NostDB single-node server

Usage:
    nostdb-server --database PATH [--listen ADDRESS] [--api-key KEY]
                  [--timeout-ms N] [--max-rows N] [--max-memory-bytes N]
                  [--max-operations N] [--max-traversals N]

The API key may instead be supplied through NOSTDB_API_KEY.
The default listen address is 127.0.0.1:7878.";

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("nostdb-server: {error}");
            ExitCode::from(2)
        }
    }
}

async fn run() -> Result<(), String> {
    let Some((listen, config)) = parse_arguments()? else {
        println!("{HELP}");
        return Ok(());
    };
    init_tracing()?;
    let state = AppState::new(config).map_err(|error| error.to_string())?;
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|error| format!("cannot bind {listen}: {error}"))?;
    tracing::info!(address = %listen, "NostDB server listening");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|error| format!("HTTP server failed: {error}"))
}

fn parse_arguments() -> Result<Option<(SocketAddr, ServerConfig)>, String> {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    if arguments
        .iter()
        .any(|value| matches!(value.as_str(), "-h" | "--help"))
    {
        return Ok(None);
    }
    let mut database = None;
    let mut api_key = env::var("NOSTDB_API_KEY").ok();
    let mut listen = "127.0.0.1:7878".to_owned();
    let mut timeout_ms = None;
    let mut max_rows = None;
    let mut max_memory_bytes = None;
    let mut max_operations = None;
    let mut max_traversals = None;
    let mut index = 0;
    while index < arguments.len() {
        let option = arguments[index].as_str();
        index += 1;
        let value = arguments
            .get(index)
            .ok_or_else(|| format!("{option} requires a value"))?;
        match option {
            "--database" => database = Some(value.clone()),
            "--api-key" => api_key = Some(value.clone()),
            "--listen" => listen = value.clone(),
            "--timeout-ms" => timeout_ms = Some(number(value)?),
            "--max-rows" => max_rows = Some(number(value)?),
            "--max-memory-bytes" => max_memory_bytes = Some(number(value)?),
            "--max-operations" => max_operations = Some(number(value)?),
            "--max-traversals" => max_traversals = Some(number(value)?),
            option => return Err(format!("unknown option `{option}`")),
        }
        index += 1;
    }
    let database = database.ok_or_else(|| "--database is required".to_owned())?;
    let api_key = api_key.ok_or_else(|| "--api-key or NOSTDB_API_KEY is required".to_owned())?;
    let listen = listen
        .parse::<SocketAddr>()
        .map_err(|error| format!("invalid --listen address: {error}"))?;
    let mut config = ServerConfig::new(PathBuf::from(database), api_key);
    if let Some(value) = timeout_ms {
        config.query_timeout = Duration::from_millis(value);
    }
    config.query_limits.max_rows = max_rows.unwrap_or(config.query_limits.max_rows);
    config.query_limits.max_memory_bytes =
        max_memory_bytes.unwrap_or(config.query_limits.max_memory_bytes);
    config.query_limits.max_operations =
        max_operations.unwrap_or(config.query_limits.max_operations);
    config.query_limits.max_traversals =
        max_traversals.unwrap_or(config.query_limits.max_traversals);
    Ok(Some((listen, config)))
}

fn number(value: &str) -> Result<u64, String> {
    value
        .parse()
        .map_err(|_| format!("`{value}` is not a valid non-negative integer"))
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

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!(%error, "failed to install shutdown signal handler");
    }
}
