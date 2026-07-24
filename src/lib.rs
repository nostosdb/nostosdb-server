#![forbid(unsafe_code)]
//! Authenticated single-node HTTP boundary for the NostDB Engine.

mod api;
mod catalog;
pub mod config;
pub mod daemon;
mod protocol;
mod wire;

pub use protocol::serve_database_protocol;

/// Independent version of the public HTTP protocol.
pub const SERVER_PROTOCOL_VERSION: u32 = 1;

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::middleware;
use nostdb_engine::{EmbeddedDatabase, Parameters, QueryLimits};
use tokio::sync::Mutex as AsyncMutex;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

/// Runtime configuration for one server process.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// Path to the authoritative server-mode `.nostdb` database.
    pub database_path: PathBuf,
    /// Required bearer API key. Empty keys are rejected.
    pub api_key: String,
    /// Default cooperative query limits.
    pub query_limits: QueryLimits,
    /// Wall-clock query timeout.
    pub query_timeout: Duration,
    /// Maximum JSON request body size.
    pub request_body_bytes: usize,
    /// Maximum snapshot upload size.
    pub snapshot_body_bytes: usize,
    /// Maximum concurrent retained sessions.
    pub max_sessions: usize,
    /// Maximum statements queued in one explicit transaction.
    pub max_transaction_statements: usize,
}

impl ServerConfig {
    /// Returns conservative initial limits for an explicit path and API key.
    #[must_use]
    pub fn new(database_path: PathBuf, api_key: String) -> Self {
        Self {
            database_path,
            api_key,
            query_limits: QueryLimits {
                max_rows: 10_000,
                max_memory_bytes: 64 * 1024 * 1024,
                max_operations: 10_000_000,
                max_traversals: 1_000_000,
            },
            query_timeout: Duration::from_secs(30),
            request_body_bytes: 1024 * 1024,
            snapshot_body_bytes: 1024 * 1024 * 1024,
            max_sessions: 1024,
            max_transaction_statements: 1000,
        }
    }

    fn validate(&self) -> Result<(), ServerError> {
        if self.api_key.is_empty() {
            return Err(ServerError::new("API key must not be empty"));
        }
        if self.query_timeout.is_zero()
            || self.request_body_bytes == 0
            || self.snapshot_body_bytes < self.request_body_bytes
            || self.max_sessions == 0
            || self.max_transaction_statements == 0
        {
            return Err(ServerError::new(
                "server limits must be positive and consistent",
            ));
        }
        Ok(())
    }
}

/// Configuration or database initialization failure.
#[derive(Debug)]
pub struct ServerError(String);

impl ServerError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for ServerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for ServerError {}

#[derive(Default)]
pub(crate) struct Metrics {
    pub(crate) requests: AtomicU64,
    pub(crate) queries: AtomicU64,
    pub(crate) query_errors: AtomicU64,
    pub(crate) auth_failures: AtomicU64,
    pub(crate) timeouts: AtomicU64,
    pub(crate) sessions_created: AtomicU64,
    pub(crate) snapshot_restores: AtomicU64,
}

impl Metrics {
    pub(crate) fn render(&self, active_sessions: usize) -> String {
        format!(
            concat!(
                "# TYPE nostdb_http_requests_total counter\n",
                "nostdb_http_requests_total {}\n",
                "# TYPE nostdb_queries_total counter\n",
                "nostdb_queries_total {}\n",
                "nostdb_query_errors_total {}\n",
                "nostdb_auth_failures_total {}\n",
                "nostdb_query_timeouts_total {}\n",
                "nostdb_sessions_created_total {}\n",
                "nostdb_snapshot_restores_total {}\n",
                "# TYPE nostdb_sessions_active gauge\n",
                "nostdb_sessions_active {}\n"
            ),
            self.requests.load(Ordering::Relaxed),
            self.queries.load(Ordering::Relaxed),
            self.query_errors.load(Ordering::Relaxed),
            self.auth_failures.load(Ordering::Relaxed),
            self.timeouts.load(Ordering::Relaxed),
            self.sessions_created.load(Ordering::Relaxed),
            self.snapshot_restores.load(Ordering::Relaxed),
            active_sessions,
        )
    }
}

pub(crate) struct Session {
    pub(crate) transaction: Option<Vec<(String, Parameters)>>,
}

/// Shared application state. Clone is cheap and shares all state safely.
#[derive(Clone)]
pub struct AppState {
    pub(crate) database: Arc<Mutex<Option<EmbeddedDatabase>>>,
    pub(crate) sessions: Arc<AsyncMutex<BTreeMap<String, Session>>>,
    pub(crate) metrics: Arc<Metrics>,
    pub(crate) config: Arc<ServerConfig>,
}

impl AppState {
    /// Opens or creates the configured server-mode database.
    pub fn new(config: ServerConfig) -> Result<Self, ServerError> {
        config.validate()?;
        let database = if config.database_path.exists() {
            EmbeddedDatabase::open(&config.database_path)
                .map_err(|error| ServerError::new(error.to_string()))?
        } else {
            if let Some(parent) = config
                .database_path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                std::fs::create_dir_all(parent).map_err(|error| {
                    ServerError::new(format!("cannot create database directory: {error}"))
                })?;
            }
            EmbeddedDatabase::create(&config.database_path)
                .map_err(|error| ServerError::new(error.to_string()))?
        };
        let info = database
            .info()
            .map_err(|error| ServerError::new(error.to_string()))?;
        if info.source_enabled {
            return Err(ServerError::new(
                "server mode refuses a database with a human-readable-source synchronization baseline; import an NDB-only snapshot",
            ));
        }
        Ok(Self {
            database: Arc::new(Mutex::new(Some(database))),
            sessions: Arc::new(AsyncMutex::new(BTreeMap::new())),
            metrics: Arc::new(Metrics::default()),
            config: Arc::new(config),
        })
    }
}

/// Builds the complete health and authenticated API router.
pub fn router(state: AppState) -> Router {
    let protected = api::protected_routes(
        state.config.request_body_bytes,
        state.config.snapshot_body_bytes,
    )
    .route_layer(middleware::from_fn_with_state(
        state.clone(),
        api::authenticate,
    ))
    .layer(DefaultBodyLimit::disable());
    Router::new()
        .merge(api::public_routes())
        .merge(protected)
        .layer(RequestBodyLimitLayer::new(state.config.snapshot_body_bytes))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
