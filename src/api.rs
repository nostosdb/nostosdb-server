use std::collections::BTreeMap;
use std::convert::Infallible;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path as FsPath, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures_util::stream;
use nostdb_engine::{
    CancellationToken, DatabaseError, EmbeddedDatabase, LogicalPackage, Parameters, ProjectConfig,
    QueryErrorCode, QueryLimits, StatementResult, Synchronizer, prepare, prepare_write,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tower_http::limit::RequestBodyLimitLayer;
use uuid::Uuid;

use crate::wire::{self, ErrorBody, ErrorDetail};
use crate::{AppState, SERVER_PROTOCOL_VERSION, Session};

pub(crate) fn public_routes() -> Router<AppState> {
    Router::new().route("/healthz", get(health))
}

pub(crate) fn protected_routes(
    request_body_bytes: usize,
    snapshot_body_bytes: usize,
) -> Router<AppState> {
    let request_limit = RequestBodyLimitLayer::new(request_body_bytes);
    Router::new()
        .route("/metrics", get(metrics))
        .route("/v1/query", post(query).layer(request_limit))
        .route("/v1/catalog", get(catalog))
        .route("/v1/schema", get(schema))
        .route("/v1/unresolved", get(unresolved))
        .route("/v1/sessions", post(create_session))
        .route("/v1/sessions/{id}", delete(delete_session))
        .route("/v1/sessions/{id}/begin", post(begin))
        .route(
            "/v1/sessions/{id}/query",
            post(session_query).layer(request_limit),
        )
        .route("/v1/sessions/{id}/commit", post(commit))
        .route("/v1/sessions/{id}/rollback", post(rollback))
        .route(
            "/v1/admin/snapshot",
            get(export_snapshot)
                .put(import_snapshot)
                .layer(RequestBodyLimitLayer::new(snapshot_body_bytes)),
        )
        .route(
            "/v1/admin/logical",
            get(export_logical).put(import_logical).layer(request_limit),
        )
}

pub(crate) async fn authenticate(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    state.metrics.requests.fetch_add(1, Ordering::Relaxed);
    let supplied = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if !supplied.is_some_and(|value| constant_time_equal(value, &state.config.api_key)) {
        state.metrics.auth_failures.fetch_add(1, Ordering::Relaxed);
        return ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", "invalid API key")
            .into_response();
    }
    next.run(request).await
}

fn constant_time_equal(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        difference |= usize::from(
            left.get(index).copied().unwrap_or(0) ^ right.get(index).copied().unwrap_or(0),
        );
    }
    difference == 0
}

async fn health() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "protocol_version": SERVER_PROTOCOL_VERSION,
    }))
}

async fn metrics(State(state): State<AppState>) -> Response {
    let active = state.sessions.lock().await.len();
    let mut response = state.metrics.render(active).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4"),
    );
    response
}

#[derive(Deserialize)]
struct QueryRequest {
    query: String,
    #[serde(default)]
    parameters: BTreeMap<String, Value>,
    #[serde(default)]
    stream: bool,
    #[serde(default)]
    read_only: bool,
    limits: Option<RequestLimits>,
}

#[derive(Clone, Copy, Deserialize)]
struct RequestLimits {
    max_rows: Option<u64>,
    max_memory_bytes: Option<u64>,
    max_operations: Option<u64>,
    max_traversals: Option<u64>,
    timeout_ms: Option<u64>,
}

impl RequestLimits {
    fn effective(self, state: &AppState) -> Result<(QueryLimits, Duration), ApiError> {
        let configured = state.config.query_limits;
        let limits = QueryLimits {
            max_rows: self
                .max_rows
                .unwrap_or(configured.max_rows)
                .min(configured.max_rows),
            max_memory_bytes: self
                .max_memory_bytes
                .unwrap_or(configured.max_memory_bytes)
                .min(configured.max_memory_bytes),
            max_operations: self
                .max_operations
                .unwrap_or(configured.max_operations)
                .min(configured.max_operations),
            max_traversals: self
                .max_traversals
                .unwrap_or(configured.max_traversals)
                .min(configured.max_traversals),
        };
        let requested = self.timeout_ms.map(Duration::from_millis);
        let timeout = requested
            .unwrap_or(state.config.query_timeout)
            .min(state.config.query_timeout);
        if timeout.is_zero() {
            return Err(ApiError::bad_request("timeout_ms must be positive"));
        }
        Ok((limits, timeout))
    }
}

async fn query(State(state): State<AppState>, body: Bytes) -> Result<Response, ApiError> {
    let request = parse_query(&state, &body)?;
    require_read_only(&request)?;
    let parameters = wire::parameters(request.parameters).map_err(ApiError::bad_request)?;
    let (limits, timeout) = request.limits.map_or(
        Ok((state.config.query_limits, state.config.query_timeout)),
        |limits| limits.effective(&state),
    )?;
    let result = execute_one(&state, request.query, parameters, limits, timeout).await?;
    response(result, request.stream)
}

fn require_read_only(request: &QueryRequest) -> Result<(), ApiError> {
    if request.read_only {
        prepare(&request.query).map_err(|error| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "query_error",
                format!("read-only query required: {error}"),
            )
        })?;
    }
    Ok(())
}

#[derive(Deserialize)]
struct ListRequest {
    limit: Option<u64>,
}

impl ListRequest {
    fn effective_limit(&self, state: &AppState) -> usize {
        let requested = self.limit.unwrap_or(state.config.query_limits.max_rows);
        usize::try_from(requested.min(state.config.query_limits.max_rows)).unwrap_or(usize::MAX)
    }
}

async fn catalog(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let database = state.database.clone();
    let value = tokio::task::spawn_blocking(move || {
        let guard = database
            .lock()
            .map_err(|_| "database lock is poisoned".to_owned())?;
        let database = guard
            .as_ref()
            .ok_or_else(|| "database is temporarily unavailable".to_owned())?;
        let info = database.info().map_err(|error| error.to_string())?;
        let counts = database.counts().map_err(|error| error.to_string())?;
        Ok::<_, String>(json!({
            "protocol_version": SERVER_PROTOCOL_VERSION,
            "database": {
                "ndb_format_version": info.ndb_format_version,
                "schema_revision": info.schema_revision,
                "value_codec_version": info.value_codec_version,
                "checksum_codec_version": info.checksum_codec_version,
                "generation": info.generation,
                "logical_checksum": format!("{:016x}", info.logical_checksum),
                "source_managed": info.source_managed,
            },
            "counts": {
                "schemas": counts.schemas,
                "nodes": counts.nodes,
                "edges": counts.edges,
                "adjacency": counts.adjacency,
                "properties": counts.properties,
            },
        }))
    })
    .await
    .map_err(|error| ApiError::internal(error.to_string()))?
    .map_err(ApiError::internal)?;
    Ok(Json(value))
}

async fn schema(
    State(state): State<AppState>,
    request: Result<Query<ListRequest>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(request) = request.map_err(|error| ApiError::bad_request(error.to_string()))?;
    let limit = request.effective_limit(&state);
    let database = state.database.clone();
    let value = tokio::task::spawn_blocking(move || {
        let guard = database
            .lock()
            .map_err(|_| "database lock is poisoned".to_owned())?;
        let database = guard
            .as_ref()
            .ok_or_else(|| "database is temporarily unavailable".to_owned())?;
        let mut schemas = database.schemas().map_err(|error| error.to_string())?;
        let truncated = schemas.len() > limit;
        schemas.truncate(limit);
        let schemas = schemas
            .into_iter()
            .map(|schema| {
                json!({
                    "identity": schema.identity,
                    "state": schema.state,
                    "properties": schema.properties.into_iter().map(|property| json!({
                        "name": property.name,
                        "type": property.property_type,
                    })).collect::<Vec<_>>(),
                    "constraints": schema.constraints,
                })
            })
            .collect::<Vec<_>>();
        let returned = schemas.len();
        Ok::<_, String>(json!({
            "schemas": schemas,
            "returned": returned,
            "truncated": truncated,
        }))
    })
    .await
    .map_err(|error| ApiError::internal(error.to_string()))?
    .map_err(ApiError::internal)?;
    Ok(Json(value))
}

async fn unresolved(
    State(state): State<AppState>,
    request: Result<Query<ListRequest>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(request) = request.map_err(|error| ApiError::bad_request(error.to_string()))?;
    let limit = request.effective_limit(&state);
    let database = state.database.clone();
    let value = tokio::task::spawn_blocking(move || {
        let guard = database
            .lock()
            .map_err(|_| "database lock is poisoned".to_owned())?;
        let database = guard
            .as_ref()
            .ok_or_else(|| "database is temporarily unavailable".to_owned())?;
        let mut entries = database.unresolved().map_err(|error| error.to_string())?;
        let truncated = entries.len() > limit;
        entries.truncate(limit);
        let entries = entries
            .into_iter()
            .map(|entry| {
                json!({
                    "kind": entry.kind,
                    "internal_id": entry.internal_id,
                    "identity": entry.identity,
                    "state": entry.state,
                })
            })
            .collect::<Vec<_>>();
        let returned = entries.len();
        Ok::<_, String>(json!({
            "entries": entries,
            "returned": returned,
            "truncated": truncated,
        }))
    })
    .await
    .map_err(|error| ApiError::internal(error.to_string()))?
    .map_err(ApiError::internal)?;
    Ok(Json(value))
}

fn parse_query(state: &AppState, body: &[u8]) -> Result<QueryRequest, ApiError> {
    if body.len() > state.config.request_body_bytes {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            "JSON request body exceeds the configured limit",
        ));
    }
    serde_json::from_slice(body).map_err(|error| ApiError::bad_request(error.to_string()))
}

async fn execute_one(
    state: &AppState,
    query: String,
    parameters: Parameters,
    limits: QueryLimits,
    timeout: Duration,
) -> Result<StatementResult, ApiError> {
    state.metrics.queries.fetch_add(1, Ordering::Relaxed);
    let cancellation = CancellationToken::new();
    let worker_cancellation = cancellation.clone();
    let database = state.database.clone();
    let mut worker = tokio::task::spawn_blocking(move || {
        let mut guard = database
            .lock()
            .map_err(|_| "database lock is poisoned".to_owned())?;
        let database = guard
            .as_mut()
            .ok_or_else(|| "database is temporarily unavailable".to_owned())?;
        database
            .execute_limited(&query, &parameters, limits, worker_cancellation)
            .map_err(DatabaseFailure::from)
            .map_err(|error| error.to_string())
    });
    match tokio::time::timeout(timeout, &mut worker).await {
        Ok(result) => join_result(result).inspect_err(|_| {
            state.metrics.query_errors.fetch_add(1, Ordering::Relaxed);
        }),
        Err(_) => {
            cancellation.cancel();
            finish_timeout(
                state,
                worker,
                "query exceeded the configured wall-clock timeout",
            )
            .await
        }
    }
}

async fn execute_batch(
    state: &AppState,
    statements: Vec<(String, Parameters)>,
) -> Result<Vec<StatementResult>, ApiError> {
    state.metrics.queries.fetch_add(1, Ordering::Relaxed);
    let cancellation = CancellationToken::new();
    let worker_cancellation = cancellation.clone();
    let database = state.database.clone();
    let limits = state.config.query_limits;
    let mut worker = tokio::task::spawn_blocking(move || {
        let mut guard = database
            .lock()
            .map_err(|_| "database lock is poisoned".to_owned())?;
        let database = guard
            .as_mut()
            .ok_or_else(|| "database is temporarily unavailable".to_owned())?;
        database
            .execute_transaction_limited(&statements, limits, worker_cancellation)
            .map_err(DatabaseFailure::from)
            .map_err(|error| error.to_string())
    });
    match tokio::time::timeout(state.config.query_timeout, &mut worker).await {
        Ok(result) => join_result(result).inspect_err(|_| {
            state.metrics.query_errors.fetch_add(1, Ordering::Relaxed);
        }),
        Err(_) => {
            cancellation.cancel();
            finish_timeout(
                state,
                worker,
                "transaction exceeded the configured wall-clock timeout",
            )
            .await
        }
    }
}

async fn finish_timeout<T>(
    state: &AppState,
    worker: tokio::task::JoinHandle<Result<T, String>>,
    message: &'static str,
) -> Result<T, ApiError> {
    match worker.await {
        // The worker can win the race between the timer firing and cancellation.
        // Report that committed result as success; never claim a committed write
        // timed out.
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) if error.contains("query execution cancelled") => {
            state.metrics.timeouts.fetch_add(1, Ordering::Relaxed);
            Err(ApiError::new(
                StatusCode::REQUEST_TIMEOUT,
                "query_timeout",
                message,
            ))
        }
        Ok(Err(error)) => Err(database_message(error)),
        Err(error) => Err(ApiError::internal(format!(
            "query worker failed after timeout: {error}"
        ))),
    }
}

fn join_result<T>(
    result: Result<Result<T, String>, tokio::task::JoinError>,
) -> Result<T, ApiError> {
    match result {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(message)) => Err(database_message(message)),
        Err(error) => Err(ApiError::internal(format!("query worker failed: {error}"))),
    }
}

#[derive(Debug)]
struct DatabaseFailure {
    code: Option<QueryErrorCode>,
    message: String,
}

impl From<DatabaseError> for DatabaseFailure {
    fn from(error: DatabaseError) -> Self {
        match error {
            DatabaseError::Query(error) => Self {
                code: Some(error.code()),
                message: error.to_string(),
            },
            DatabaseError::Storage(error) => Self {
                code: None,
                message: error.to_string(),
            },
        }
    }
}

impl std::fmt::Display for DatabaseFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let prefix = match self.code {
            Some(QueryErrorCode::ResourceLimit) => "resource_limit|",
            Some(_) => "query_error|",
            None => "database_error|",
        };
        write!(formatter, "{prefix}{}", self.message)
    }
}

fn database_message(message: String) -> ApiError {
    if let Some(message) = message.strip_prefix("resource_limit|") {
        ApiError::new(StatusCode::TOO_MANY_REQUESTS, "resource_limit", message)
    } else if let Some(message) = message.strip_prefix("query_error|") {
        ApiError::new(StatusCode::BAD_REQUEST, "query_error", message)
    } else if let Some(message) = message.strip_prefix("database_error|") {
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "database_error", message)
    } else {
        ApiError::internal(message)
    }
}

fn response(result: StatementResult, streaming: bool) -> Result<Response, ApiError> {
    if streaming {
        if let StatementResult::Read(result) = &result {
            let mut chunks = Vec::with_capacity(result.rows.len() + 1);
            chunks.push(Bytes::from(
                serde_json::to_vec(&json!({
                    "columns": result.columns,
                    "ordered": result.ordered,
                }))
                .map_err(|error| ApiError::internal(error.to_string()))?,
            ));
            chunks.push(Bytes::from_static(b"\n"));
            for row in &result.rows {
                chunks.push(Bytes::from(
                    serde_json::to_vec(&wire::row_object(&result.columns, row))
                        .map_err(|error| ApiError::internal(error.to_string()))?,
                ));
                chunks.push(Bytes::from_static(b"\n"));
            }
            let stream = stream::iter(chunks.into_iter().map(Ok::<_, Infallible>));
            let mut response = Response::new(Body::from_stream(stream));
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/x-ndjson"),
            );
            return Ok(response);
        }
    }
    Ok(Json(wire::statement(&result)).into_response())
}

#[derive(Serialize)]
struct SessionCreated {
    session_id: String,
}

async fn create_session(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let mut sessions = state.sessions.lock().await;
    if sessions.len() >= state.config.max_sessions {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "session_limit",
            "maximum session count reached",
        ));
    }
    let id = Uuid::new_v4().to_string();
    sessions.insert(id.clone(), Session { transaction: None });
    state
        .metrics
        .sessions_created
        .fetch_add(1, Ordering::Relaxed);
    Ok((StatusCode::CREATED, Json(SessionCreated { session_id: id })))
}

async fn delete_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    state
        .sessions
        .lock()
        .await
        .remove(&id)
        .ok_or_else(session_not_found)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn begin(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let mut sessions = state.sessions.lock().await;
    let session = sessions.get_mut(&id).ok_or_else(session_not_found)?;
    if session.transaction.is_some() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "transaction_active",
            "session already has an active transaction",
        ));
    }
    session.transaction = Some(Vec::new());
    Ok(Json(json!({"status": "active"})))
}

async fn session_query(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let request = parse_query(&state, &body)?;
    require_read_only(&request)?;
    let parameters = wire::parameters(request.parameters).map_err(ApiError::bad_request)?;
    let mut sessions = state.sessions.lock().await;
    let session = sessions.get_mut(&id).ok_or_else(session_not_found)?;
    if let Some(statements) = &mut session.transaction {
        if request.limits.is_some() {
            return Err(ApiError::bad_request(
                "per-request limits are not accepted inside a transaction; the server transaction budget applies at commit",
            ));
        }
        if statements.len() >= state.config.max_transaction_statements {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "transaction_statement_limit",
                "transaction statement limit reached",
            ));
        }
        if prepare(&request.query).is_err() && prepare_write(&request.query).is_err() {
            return Err(ApiError::bad_request("query is not valid or supported"));
        }
        statements.push((request.query, parameters));
        return Ok(Json(json!({
            "status": "queued",
            "statement_count": statements.len(),
        }))
        .into_response());
    }
    drop(sessions);
    let (limits, timeout) = request.limits.map_or(
        Ok((state.config.query_limits, state.config.query_timeout)),
        |limits| limits.effective(&state),
    )?;
    let result = execute_one(&state, request.query, parameters, limits, timeout).await?;
    response(result, request.stream)
}

async fn commit(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let statements = {
        let mut sessions = state.sessions.lock().await;
        let session = sessions.get_mut(&id).ok_or_else(session_not_found)?;
        session.transaction.take().ok_or_else(|| {
            ApiError::new(
                StatusCode::CONFLICT,
                "no_transaction",
                "session has no active transaction",
            )
        })?
    };
    let results = execute_batch(&state, statements).await?;
    Ok(Json(json!({
        "status": "committed",
        "results": results.iter().map(wire::statement).collect::<Vec<_>>(),
    })))
}

async fn rollback(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let mut sessions = state.sessions.lock().await;
    let session = sessions.get_mut(&id).ok_or_else(session_not_found)?;
    session.transaction.take().ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "no_transaction",
            "session has no active transaction",
        )
    })?;
    Ok(Json(json!({"status": "rolled_back"})))
}

fn session_not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "session_not_found",
        "session does not exist",
    )
}

async fn export_snapshot(State(state): State<AppState>) -> Result<Response, ApiError> {
    let database = state.database.clone();
    let path = state.config.database_path.clone();
    let bytes = tokio::task::spawn_blocking(move || {
        let mut guard = database
            .lock()
            .map_err(|_| "database lock is poisoned".to_owned())?;
        let database = guard
            .as_mut()
            .ok_or_else(|| "database is temporarily unavailable".to_owned())?;
        database.checkpoint().map_err(|error| error.to_string())?;
        fs::read(path).map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| ApiError::internal(error.to_string()))?
    .map_err(ApiError::internal)?;
    let mut response = Response::new(Body::from(bytes));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/vnd.nostdb.ndb"),
    );
    response
        .headers_mut()
        .insert("x-nostdb-ndb-format", HeaderValue::from_static("0"));
    Ok(response)
}

async fn import_snapshot(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    if body.is_empty() {
        return Err(ApiError::bad_request("snapshot body is empty"));
    }
    let state_for_worker = state.clone();
    let size = body.len();
    tokio::task::spawn_blocking(move || restore_snapshot(&state_for_worker, &body))
        .await
        .map_err(|error| ApiError::internal(error.to_string()))??;
    state
        .metrics
        .snapshot_restores
        .fetch_add(1, Ordering::Relaxed);
    Ok(Json(json!({"status": "restored", "bytes": size})))
}

fn restore_snapshot(state: &AppState, body: &[u8]) -> Result<(), ApiError> {
    let target = &state.config.database_path;
    let suffix = Uuid::new_v4();
    let temporary = target.with_extension(format!("ndb.import-{suffix}"));
    let backup = target.with_extension(format!("ndb.backup-{suffix}"));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|error| ApiError::internal(error.to_string()))?;
        file.write_all(body)
            .and_then(|()| file.sync_all())
            .map_err(|error| ApiError::internal(error.to_string()))?;
        drop(file);

        let mut candidate = EmbeddedDatabase::open(&temporary).map_err(snapshot_incompatible)?;
        if !candidate.check().map_err(snapshot_incompatible)?.is_valid() {
            return Err(snapshot_incompatible("snapshot integrity check failed"));
        }
        candidate
            .adopt_server_authority()
            .map_err(snapshot_incompatible)?;
        candidate.checkpoint().map_err(snapshot_incompatible)?;
        drop(candidate);

        let mut guard = state
            .database
            .lock()
            .map_err(|_| ApiError::internal("database lock is poisoned"))?;
        let mut current = guard
            .take()
            .ok_or_else(|| ApiError::internal("database is temporarily unavailable"))?;
        current.checkpoint().map_err(snapshot_incompatible)?;
        drop(current);

        if let Err(error) = fs::rename(target, &backup) {
            *guard = EmbeddedDatabase::open(target).ok();
            return Err(ApiError::internal(format!(
                "cannot preserve current snapshot: {error}"
            )));
        }
        if let Err(error) = fs::rename(&temporary, target) {
            let _ = fs::rename(&backup, target);
            *guard = EmbeddedDatabase::open(target).ok();
            return Err(ApiError::internal(format!(
                "cannot install compatible snapshot: {error}"
            )));
        }
        match EmbeddedDatabase::open(target) {
            Ok(database) => {
                *guard = Some(database);
                fs::remove_file(&backup).map_err(|error| {
                    ApiError::internal(format!(
                        "snapshot restored but backup cleanup failed: {error}"
                    ))
                })?;
                Ok(())
            }
            Err(error) => {
                let _ = fs::remove_file(target);
                let _ = fs::rename(&backup, target);
                *guard = EmbeddedDatabase::open(target).ok();
                Err(ApiError::internal(format!(
                    "restored snapshot could not reopen: {error}"
                )))
            }
        }
    })();
    if temporary.exists() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn snapshot_incompatible(error: impl std::fmt::Display) -> ApiError {
    ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "incompatible_snapshot",
        error.to_string(),
    )
}

#[derive(Serialize, Deserialize)]
struct LogicalPackageBody {
    package_version: u32,
    language_version: u32,
    config: String,
    modules: Vec<LogicalModuleBody>,
}

#[derive(Serialize, Deserialize)]
struct LogicalModuleBody {
    path: String,
    stable_module_id: String,
    source: String,
}

impl From<LogicalPackage> for LogicalPackageBody {
    fn from(package: LogicalPackage) -> Self {
        Self {
            package_version: package.package_version,
            language_version: 1,
            config: package.config,
            modules: package
                .modules
                .into_iter()
                .map(|module| LogicalModuleBody {
                    path: module.path,
                    stable_module_id: module.module_id,
                    source: module.source,
                })
                .collect(),
        }
    }
}

async fn export_logical(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let database = state.database.clone();
    let package = tokio::task::spawn_blocking(move || {
        let guard = database
            .lock()
            .map_err(|_| "database lock is poisoned".to_owned())?;
        let database = guard
            .as_ref()
            .ok_or_else(|| "database is temporarily unavailable".to_owned())?;
        database.export_logical().map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| ApiError::internal(error.to_string()))?
    .map_err(|error| {
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "logical_export_failed",
            error,
        )
    })?;
    Ok(Json(
        serde_json::to_value(LogicalPackageBody::from(package))
            .map_err(|error| ApiError::internal(error.to_string()))?,
    ))
}

async fn import_logical(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    if body.len() > state.config.request_body_bytes {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            "logical package exceeds the configured JSON body limit",
        ));
    }
    let package: LogicalPackageBody =
        serde_json::from_slice(&body).map_err(|error| ApiError::bad_request(error.to_string()))?;
    if package.package_version != 1 {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unsupported_logical_package",
            "logical package_version must be 1",
        ));
    }
    if package.language_version != 1 {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unsupported_logical_package",
            "logical language_version must be 1",
        ));
    }
    let state_for_worker = state.clone();
    let modules = package.modules.len();
    tokio::task::spawn_blocking(move || import_logical_package(&state_for_worker, package))
        .await
        .map_err(|error| ApiError::internal(error.to_string()))??;
    Ok(Json(json!({"status": "imported", "modules": modules})))
}

fn import_logical_package(state: &AppState, package: LogicalPackageBody) -> Result<(), ApiError> {
    let parent = state
        .config
        .database_path
        .parent()
        .unwrap_or_else(|| FsPath::new("."));
    let directory = parent.join(format!(".nostdb-logical-import-{}", Uuid::new_v4()));
    fs::create_dir(&directory).map_err(|error| ApiError::internal(error.to_string()))?;
    let result = (|| {
        fs::write(directory.join("nostdb.toml"), package.config)
            .map_err(|error| ApiError::internal(error.to_string()))?;
        let config = ProjectConfig::load(&directory).map_err(|error| {
            ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_logical_package",
                error.to_string(),
            )
        })?;
        let mut seen = std::collections::BTreeSet::new();
        for module in package.modules {
            let path = safe_relative(&module.path)?;
            if !seen.insert(path.clone()) {
                return Err(ApiError::bad_request(
                    "logical package repeats a module path",
                ));
            }
            let module_id = module
                .stable_module_id
                .parse::<nostdb_engine::StableModuleId>()
                .map_err(|_| {
                    ApiError::bad_request(format!("invalid stable_module_id for {}", module.path))
                })?;
            if config.module_id(&path) != Some(module_id) {
                return Err(ApiError::bad_request(format!(
                    "stable_module_id does not match nostdb.toml for {}",
                    module.path
                )));
            }
            let target = directory.join(path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| ApiError::internal(error.to_string()))?;
            }
            fs::write(target, module.source)
                .map_err(|error| ApiError::internal(error.to_string()))?;
        }
        let candidate_path = directory.join("candidate.ndb");
        Synchronizer::default()
            .sync(&directory, &candidate_path)
            .map_err(|error| {
                ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "invalid_logical_package",
                    error.to_string(),
                )
            })?;
        let mut candidate =
            EmbeddedDatabase::open(&candidate_path).map_err(snapshot_incompatible)?;
        candidate
            .adopt_server_authority()
            .map_err(snapshot_incompatible)?;
        candidate.checkpoint().map_err(snapshot_incompatible)?;
        drop(candidate);
        let bytes =
            fs::read(candidate_path).map_err(|error| ApiError::internal(error.to_string()))?;
        restore_snapshot(state, &bytes)
    })();
    if let Err(error) = fs::remove_dir_all(&directory) {
        tracing::warn!(
            path = %directory.display(),
            operation_succeeded = result.is_ok(),
            %error,
            "logical import temporary-directory cleanup failed"
        );
    }
    result
}

fn safe_relative(value: &str) -> Result<PathBuf, ApiError> {
    let path = FsPath::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || path.extension().and_then(|value| value.to_str()) != Some("nostdb")
    {
        return Err(ApiError::bad_request(format!(
            "invalid logical module path `{value}`"
        )));
    }
    Ok(path.to_path_buf())
}

struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                error: ErrorDetail {
                    code: self.code,
                    message: self.message,
                },
            }),
        )
            .into_response()
    }
}
