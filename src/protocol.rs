use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use nostdb_client::{
    ClientFrame, ClientRequest, ClientRole, DATABASE_PROTOCOL_VERSION, ErrorCode, MAX_FRAME_BYTES,
    SNAPSHOT_CHUNK_BYTES, ServerFrame, ServerResponse,
};
use nostdb_engine::{CancellationToken, StatementResult};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc, watch};
use tokio::task::JoinSet;

use crate::daemon::{DatabaseDaemon, ProtocolFailure};
use crate::{ServerError, wire};

const RESPONSE_QUEUE: usize = 8;

struct ConnectionState {
    role: ClientRole,
    selected_database_id: Option<String>,
    transaction: Option<Vec<QueuedStatement>>,
    restore: Option<SnapshotUpload>,
    last_request_id: u64,
}

struct QueuedStatement {
    query: String,
    parameters: BTreeMap<String, serde_json::Value>,
    read_only: bool,
}

struct SnapshotUpload {
    database: String,
    total_bytes: u64,
    next_sequence: u64,
    bytes: Vec<u8>,
}

/// Serves database protocol connections until the shutdown future resolves.
pub async fn serve_database_protocol(
    listener: TcpListener,
    daemon: Arc<DatabaseDaemon>,
    shutdown: impl Future<Output = ()>,
) -> Result<(), ServerError> {
    let maximum = daemon
        .config()
        .limits
        .max_connections
        .min(daemon.config().limits.max_sessions);
    let semaphore = Arc::new(Semaphore::new(maximum));
    let (shutdown_sender, shutdown_receiver) = watch::channel(false);
    let mut shutdown = Box::pin(shutdown);
    let mut connections = JoinSet::new();

    loop {
        let permit = tokio::select! {
            () = &mut shutdown => break,
            permit = semaphore.clone().acquire_owned() => permit.map_err(|_| {
                ServerError::new("connection semaphore closed unexpectedly")
            })?,
        };
        let accepted = tokio::select! {
            () = &mut shutdown => {
                drop(permit);
                break;
            }
            accepted = listener.accept() => accepted,
        };
        let (stream, peer) = accepted.map_err(|error| {
            ServerError::new(format!("database protocol accept failed: {error}"))
        })?;
        stream.set_nodelay(true).map_err(|error| {
            ServerError::new(format!("cannot configure client socket: {error}"))
        })?;
        let daemon = daemon.clone();
        let receiver = shutdown_receiver.clone();
        connections.spawn(async move {
            let _permit = permit;
            if let Err(error) = handle_connection(stream, daemon, receiver).await {
                tracing::debug!(%peer, %error, "database protocol connection closed with error");
            }
        });
        while connections.try_join_next().is_some() {}
    }

    let _ = shutdown_sender.send(true);
    let graceful = async { while connections.join_next().await.is_some() {} };
    if tokio::time::timeout(std::time::Duration::from_secs(5), graceful)
        .await
        .is_err()
    {
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    }
    Ok(())
}

async fn handle_connection(
    mut stream: TcpStream,
    daemon: Arc<DatabaseDaemon>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), String> {
    let hello = read_async_frame::<_, ClientFrame>(&mut stream)
        .await
        .map_err(|error| error.to_string())?;
    let (protocol_version, credential) = match hello.request {
        ClientRequest::Hello {
            protocol_version,
            credential,
            ..
        } => (protocol_version, credential),
        _ => {
            write_async_frame(
                &mut stream,
                &failure_frame(
                    hello.request_id,
                    ProtocolFailure::new(
                        ErrorCode::ProtocolViolation,
                        "the first frame must be hello",
                    ),
                ),
            )
            .await
            .map_err(|error| error.to_string())?;
            return Ok(());
        }
    };
    if hello.request_id == 0 {
        return Err("request identifiers must be positive".to_owned());
    }
    if protocol_version != DATABASE_PROTOCOL_VERSION {
        write_async_frame(
            &mut stream,
            &failure_frame(
                hello.request_id,
                ProtocolFailure::new(
                    ErrorCode::UnsupportedProtocol,
                    format!(
                        "client requested protocol {protocol_version}; server supports exactly {DATABASE_PROTOCOL_VERSION}"
                    ),
                ),
            ),
        )
        .await
        .map_err(|error| error.to_string())?;
        return Ok(());
    }
    let Some(role) = daemon.authenticate(&credential) else {
        write_async_frame(
            &mut stream,
            &failure_frame(
                hello.request_id,
                ProtocolFailure::new(ErrorCode::AuthenticationFailed, "credential is invalid"),
            ),
        )
        .await
        .map_err(|error| error.to_string())?;
        return Ok(());
    };
    write_async_frame(
        &mut stream,
        &ServerFrame {
            request_id: hello.request_id,
            response: ServerResponse::Hello {
                protocol_version: DATABASE_PROTOCOL_VERSION,
                server_version: env!("CARGO_PKG_VERSION").to_owned(),
                role,
                max_frame_bytes: MAX_FRAME_BYTES as u64,
            },
        },
    )
    .await
    .map_err(|error| error.to_string())?;

    let (mut reader, mut writer) = stream.into_split();
    let (sender, mut responses) = mpsc::channel::<ServerFrame>(RESPONSE_QUEUE);
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = responses.recv().await {
            write_async_frame(&mut writer, &frame).await?;
        }
        Ok::<(), io::Error>(())
    });
    let active = Arc::new(Mutex::new(BTreeMap::<u64, CancellationToken>::new()));
    let mut tasks = JoinSet::new();
    let mut state = ConnectionState {
        role,
        selected_database_id: None,
        transaction: None,
        restore: None,
        last_request_id: hello.request_id,
    };

    loop {
        let frame = tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_ok() && *shutdown.borrow() {
                    break;
                }
                continue;
            }
            frame = read_async_frame::<_, ClientFrame>(&mut reader) => match frame {
                Ok(frame) => frame,
                Err(error) if matches!(error.kind(), io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset) => break,
                Err(error) => return Err(error.to_string()),
            },
        };
        if frame.request_id <= state.last_request_id {
            send_failure(
                &sender,
                frame.request_id,
                ProtocolFailure::new(
                    ErrorCode::ProtocolViolation,
                    "request identifiers must be positive and strictly increasing",
                ),
            )
            .await?;
            continue;
        }
        state.last_request_id = frame.request_id;
        dispatch(
            frame,
            &mut state,
            daemon.clone(),
            sender.clone(),
            active.clone(),
            &mut tasks,
        )
        .await?;
        while tasks.try_join_next().is_some() {}
    }

    if let Ok(tokens) = active.lock() {
        for token in tokens.values() {
            token.cancel();
        }
    }
    while tasks.join_next().await.is_some() {}
    drop(sender);
    writer_task
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
}

async fn dispatch(
    frame: ClientFrame,
    state: &mut ConnectionState,
    daemon: Arc<DatabaseDaemon>,
    sender: mpsc::Sender<ServerFrame>,
    active: Arc<Mutex<BTreeMap<u64, CancellationToken>>>,
    tasks: &mut JoinSet<()>,
) -> Result<(), String> {
    let request_id = frame.request_id;
    if state.role != ClientRole::Admin && is_admin_request(&frame.request) {
        return send_failure(
            &sender,
            request_id,
            ProtocolFailure::new(
                ErrorCode::PermissionDenied,
                "administrative credential is required",
            ),
        )
        .await;
    }
    if state.transaction.is_some() && changes_database_context(&frame.request) {
        return send_failure(
            &sender,
            request_id,
            ProtocolFailure::new(
                ErrorCode::ProtocolViolation,
                "Database selection and lifecycle changes are unavailable during a transaction",
            ),
        )
        .await;
    }
    match frame.request {
        ClientRequest::Hello { .. } => {
            send_failure(
                &sender,
                request_id,
                ProtocolFailure::new(
                    ErrorCode::ProtocolViolation,
                    "hello may only be sent as the first frame",
                ),
            )
            .await
        }
        ClientRequest::Ping => send_response(&sender, request_id, ServerResponse::Pong).await,
        ClientRequest::SelectDatabase { database } => match daemon.select_database(&database) {
            Ok(summary) => {
                state.selected_database_id = Some(summary.id.clone());
                send_response(
                    &sender,
                    request_id,
                    ServerResponse::DatabaseSelected { database: summary },
                )
                .await
            }
            Err(error) => send_failure(&sender, request_id, error).await,
        },
        ClientRequest::Query {
            query,
            parameters,
            read_only,
            stream,
            limits,
        } => {
            let Some(database_id) = state.selected_database_id.clone() else {
                return send_failure(
                    &sender,
                    request_id,
                    ProtocolFailure::new(
                        ErrorCode::DatabaseNotSelected,
                        "select a Database before querying",
                    ),
                )
                .await;
            };
            if let Some(transaction) = &mut state.transaction {
                if stream {
                    return send_failure(
                        &sender,
                        request_id,
                        ProtocolFailure::new(
                            ErrorCode::ProtocolViolation,
                            "streaming is unavailable while a transaction is being queued",
                        ),
                    )
                    .await;
                }
                if limits.is_some() {
                    return send_failure(
                        &sender,
                        request_id,
                        ProtocolFailure::new(
                            ErrorCode::ProtocolViolation,
                            "explicit transactions use the server transaction budget",
                        ),
                    )
                    .await;
                }
                if transaction.len() >= daemon.config().limits.max_transaction_statements {
                    return send_failure(
                        &sender,
                        request_id,
                        ProtocolFailure::new(
                            ErrorCode::ResourceLimit,
                            "transaction statement limit exceeded",
                        ),
                    )
                    .await;
                }
                transaction.push(QueuedStatement {
                    query,
                    parameters,
                    read_only,
                });
                return send_response(
                    &sender,
                    request_id,
                    ServerResponse::Queued {
                        statements: transaction.len() as u64,
                    },
                )
                .await;
            }
            let token = CancellationToken::new();
            insert_active(&active, request_id, token.clone())?;
            let timeout = daemon.config().query_timeout();
            tasks.spawn(async move {
                let daemon_for_query = daemon.clone();
                let query_token = token.clone();
                let mut worker = tokio::task::spawn_blocking(move || {
                    daemon_for_query.execute_selected(
                        &database_id,
                        &query,
                        parameters,
                        read_only,
                        limits,
                        query_token,
                    )
                });
                let result = match tokio::time::timeout(timeout, &mut worker).await {
                    Ok(Ok(result)) => result,
                    Ok(Err(error)) => Err(internal_failure(error.to_string())),
                    Err(_) => {
                        token.cancel();
                        match worker.await {
                            Ok(Ok(value)) => Ok(value),
                            Ok(Err(_)) => Err(ProtocolFailure::new(
                                ErrorCode::ResourceLimit,
                                "query timeout exceeded",
                            )),
                            Err(error) => Err(internal_failure(error.to_string())),
                        }
                    }
                };
                remove_active(&active, request_id);
                match result {
                    Ok(result) => {
                        let _ = send_statement(&sender, request_id, result, stream).await;
                    }
                    Err(error) => {
                        let _ = send_failure(&sender, request_id, error).await;
                    }
                }
            });
            Ok(())
        }
        ClientRequest::Begin => {
            if state.transaction.is_some() {
                send_failure(
                    &sender,
                    request_id,
                    ProtocolFailure::new(
                        ErrorCode::TransactionAlreadyActive,
                        "a transaction is already active",
                    ),
                )
                .await
            } else if state.selected_database_id.is_none() {
                send_failure(
                    &sender,
                    request_id,
                    ProtocolFailure::new(
                        ErrorCode::DatabaseNotSelected,
                        "select a Database before beginning a transaction",
                    ),
                )
                .await
            } else {
                state.transaction = Some(Vec::new());
                send_response(
                    &sender,
                    request_id,
                    ServerResponse::Transaction {
                        state: "begun".to_owned(),
                        results: Vec::new(),
                    },
                )
                .await
            }
        }
        ClientRequest::Commit => {
            let Some(statements) = state.transaction.take() else {
                return send_failure(
                    &sender,
                    request_id,
                    ProtocolFailure::new(ErrorCode::NoTransaction, "no transaction is active"),
                )
                .await;
            };
            let Some(database_id) = state.selected_database_id.clone() else {
                return send_failure(
                    &sender,
                    request_id,
                    ProtocolFailure::new(
                        ErrorCode::DatabaseNotSelected,
                        "selected Database was cleared",
                    ),
                )
                .await;
            };
            let invalid_read_only = statements.iter().find(|statement| {
                statement.read_only && nostdb_engine::prepare_write(&statement.query).is_ok()
            });
            if invalid_read_only.is_some() {
                return send_failure(
                    &sender,
                    request_id,
                    ProtocolFailure::new(
                        ErrorCode::QueryError,
                        "read_only request rejected a mutating query",
                    ),
                )
                .await;
            }
            let statements = statements
                .into_iter()
                .map(|statement| (statement.query, statement.parameters))
                .collect();
            let token = CancellationToken::new();
            insert_active(&active, request_id, token.clone())?;
            let timeout = daemon.config().query_timeout();
            tasks.spawn(async move {
                let daemon_for_query = daemon.clone();
                let query_token = token.clone();
                let mut worker = tokio::task::spawn_blocking(move || {
                    daemon_for_query.execute_selected_transaction(
                        &database_id,
                        statements,
                        query_token,
                    )
                });
                let result = match tokio::time::timeout(timeout, &mut worker).await {
                    Ok(Ok(result)) => result,
                    Ok(Err(error)) => Err(internal_failure(error.to_string())),
                    Err(_) => {
                        token.cancel();
                        match worker.await {
                            Ok(Ok(value)) => Ok(value),
                            Ok(Err(_)) => Err(ProtocolFailure::new(
                                ErrorCode::ResourceLimit,
                                "transaction timeout exceeded",
                            )),
                            Err(error) => Err(internal_failure(error.to_string())),
                        }
                    }
                };
                remove_active(&active, request_id);
                match result {
                    Ok(results) => {
                        let results = results.iter().map(wire::statement).collect::<Vec<_>>();
                        let _ = send_response(
                            &sender,
                            request_id,
                            ServerResponse::Transaction {
                                state: "committed".to_owned(),
                                results,
                            },
                        )
                        .await;
                    }
                    Err(error) => {
                        let _ = send_failure(&sender, request_id, error).await;
                    }
                }
            });
            Ok(())
        }
        ClientRequest::Rollback => {
            if state.transaction.take().is_none() {
                send_failure(
                    &sender,
                    request_id,
                    ProtocolFailure::new(ErrorCode::NoTransaction, "no transaction is active"),
                )
                .await
            } else {
                send_response(
                    &sender,
                    request_id,
                    ServerResponse::Transaction {
                        state: "rolled_back".to_owned(),
                        results: Vec::new(),
                    },
                )
                .await
            }
        }
        ClientRequest::Cancel { target_request_id } => {
            let token = active
                .lock()
                .map_err(|_| "active request lock is poisoned".to_owned())?
                .get(&target_request_id)
                .cloned();
            if let Some(token) = token {
                token.cancel();
                send_response(
                    &sender,
                    request_id,
                    ServerResponse::Cancelled { target_request_id },
                )
                .await
            } else {
                send_failure(
                    &sender,
                    request_id,
                    ProtocolFailure::new(
                        ErrorCode::ProtocolViolation,
                        "target request is not active on this connection",
                    ),
                )
                .await
            }
        }
        ClientRequest::DatabaseCreate { name } => {
            respond_result(
                &sender,
                request_id,
                daemon.create_database(&name),
                |database| ServerResponse::DatabaseCreated { database },
            )
            .await
        }
        ClientRequest::DatabaseList => {
            respond_result(&sender, request_id, daemon.list_databases(), |databases| {
                ServerResponse::DatabaseList { databases }
            })
            .await
        }
        ClientRequest::DatabaseInspect { database } => {
            respond_result(
                &sender,
                request_id,
                daemon.inspect_database(&database),
                |database| ServerResponse::DatabaseInfo { database },
            )
            .await
        }
        ClientRequest::DatabaseRename { database, new_name } => {
            let result = daemon.rename_database(&database, &new_name);
            respond_result(&sender, request_id, result, |database| {
                ServerResponse::DatabaseRenamed { database }
            })
            .await
        }
        ClientRequest::DatabaseDrop {
            database,
            confirm_name,
        } => {
            let result = daemon.drop_database(&database, &confirm_name);
            if let Ok(dropped) = &result {
                if state.selected_database_id.as_deref() == Some(dropped.id.as_str()) {
                    state.selected_database_id = None;
                    state.transaction = None;
                }
            }
            respond_result(&sender, request_id, result, |database| {
                ServerResponse::DatabaseDropped {
                    database_id: database.id,
                    name: database.name,
                }
            })
            .await
        }
        ClientRequest::SnapshotExport { database } => {
            let daemon = daemon.clone();
            tasks.spawn(async move {
                let result = tokio::task::spawn_blocking(move || daemon.export_snapshot(&database))
                    .await
                    .map_err(|error| internal_failure(error.to_string()));
                match result {
                    Ok(Ok(bytes)) => {
                        let _ = send_snapshot(&sender, request_id, &bytes).await;
                    }
                    Ok(Err(error)) | Err(error) => {
                        let _ = send_failure(&sender, request_id, error).await;
                    }
                }
            });
            Ok(())
        }
        ClientRequest::SnapshotRestoreBegin {
            database,
            total_bytes,
        } => {
            if state.restore.is_some() {
                return send_failure(
                    &sender,
                    request_id,
                    ProtocolFailure::new(
                        ErrorCode::ProtocolViolation,
                        "a snapshot upload is already active",
                    ),
                )
                .await;
            }
            if total_bytes == 0 || total_bytes > daemon.config().limits.max_snapshot_bytes {
                return send_failure(
                    &sender,
                    request_id,
                    ProtocolFailure::new(
                        ErrorCode::RequestTooLarge,
                        "snapshot size is zero or exceeds max_snapshot_bytes",
                    ),
                )
                .await;
            }
            if let Err(error) = daemon.select_database(&database) {
                return send_failure(&sender, request_id, error).await;
            }
            state.restore = Some(SnapshotUpload {
                database,
                total_bytes,
                next_sequence: 0,
                bytes: Vec::new(),
            });
            send_response(
                &sender,
                request_id,
                ServerResponse::SnapshotRestore {
                    state: "ready".to_owned(),
                    bytes: 0,
                },
            )
            .await
        }
        ClientRequest::SnapshotRestoreChunk { sequence, data } => {
            let Some(upload) = &mut state.restore else {
                return send_failure(
                    &sender,
                    request_id,
                    ProtocolFailure::new(
                        ErrorCode::ProtocolViolation,
                        "no snapshot upload is active",
                    ),
                )
                .await;
            };
            if sequence != upload.next_sequence {
                return send_failure(
                    &sender,
                    request_id,
                    ProtocolFailure::new(
                        ErrorCode::ProtocolViolation,
                        "snapshot chunk sequence is not contiguous",
                    ),
                )
                .await;
            }
            let chunk = match BASE64.decode(data) {
                Ok(chunk) => chunk,
                Err(error) => {
                    return send_failure(
                        &sender,
                        request_id,
                        ProtocolFailure::new(
                            ErrorCode::ProtocolViolation,
                            format!("snapshot chunk is not valid base64: {error}"),
                        ),
                    )
                    .await;
                }
            };
            if chunk.is_empty() || chunk.len() > SNAPSHOT_CHUNK_BYTES {
                return send_failure(
                    &sender,
                    request_id,
                    ProtocolFailure::new(
                        ErrorCode::RequestTooLarge,
                        "snapshot chunk is empty or exceeds the chunk bound",
                    ),
                )
                .await;
            }
            let next_size = upload.bytes.len().saturating_add(chunk.len());
            if next_size as u64 > upload.total_bytes {
                return send_failure(
                    &sender,
                    request_id,
                    ProtocolFailure::new(
                        ErrorCode::RequestTooLarge,
                        "snapshot chunks exceed declared total_bytes",
                    ),
                )
                .await;
            }
            upload.bytes.extend_from_slice(&chunk);
            upload.next_sequence += 1;
            send_response(
                &sender,
                request_id,
                ServerResponse::SnapshotRestore {
                    state: "chunk_accepted".to_owned(),
                    bytes: upload.bytes.len() as u64,
                },
            )
            .await
        }
        ClientRequest::SnapshotRestoreCommit => {
            let Some(upload) = state.restore.take() else {
                return send_failure(
                    &sender,
                    request_id,
                    ProtocolFailure::new(
                        ErrorCode::ProtocolViolation,
                        "no snapshot upload is active",
                    ),
                )
                .await;
            };
            if upload.bytes.len() as u64 != upload.total_bytes {
                state.restore = Some(upload);
                return send_failure(
                    &sender,
                    request_id,
                    ProtocolFailure::new(
                        ErrorCode::ProtocolViolation,
                        "snapshot upload byte count does not match total_bytes",
                    ),
                )
                .await;
            }
            let bytes = upload.bytes.len() as u64;
            let result = daemon.restore_snapshot(&upload.database, &upload.bytes);
            respond_result(&sender, request_id, result, |()| {
                ServerResponse::SnapshotRestore {
                    state: "restored".to_owned(),
                    bytes,
                }
            })
            .await
        }
        ClientRequest::SnapshotRestoreAbort => {
            let bytes = state
                .restore
                .take()
                .map_or(0, |upload| upload.bytes.len() as u64);
            send_response(
                &sender,
                request_id,
                ServerResponse::SnapshotRestore {
                    state: "aborted".to_owned(),
                    bytes,
                },
            )
            .await
        }
        ClientRequest::LogicalExport { database } => {
            respond_result(
                &sender,
                request_id,
                daemon.export_logical(&database),
                |package| ServerResponse::LogicalPackage { package },
            )
            .await
        }
        ClientRequest::LogicalImport { database, package } => {
            respond_result(
                &sender,
                request_id,
                daemon.import_logical(&database, package),
                |modules| ServerResponse::LogicalImported { modules },
            )
            .await
        }
    }
}

async fn send_statement(
    sender: &mpsc::Sender<ServerFrame>,
    request_id: u64,
    result: StatementResult,
    stream: bool,
) -> Result<(), String> {
    match (stream, result) {
        (true, StatementResult::Read(result)) => {
            let rows = result.rows.len() as u64;
            send_response(
                sender,
                request_id,
                ServerResponse::StreamStart {
                    columns: result.columns,
                    ordered: result.ordered,
                },
            )
            .await?;
            for row in result.rows {
                send_response(
                    sender,
                    request_id,
                    ServerResponse::StreamRow {
                        row: row.iter().map(wire::query_value_json).collect(),
                    },
                )
                .await?;
            }
            send_response(sender, request_id, ServerResponse::StreamEnd { rows }).await
        }
        (_, result) => {
            send_response(
                sender,
                request_id,
                ServerResponse::Result {
                    statement: wire::statement(&result),
                },
            )
            .await
        }
    }
}

async fn send_snapshot(
    sender: &mpsc::Sender<ServerFrame>,
    request_id: u64,
    bytes: &[u8],
) -> Result<(), String> {
    send_response(
        sender,
        request_id,
        ServerResponse::SnapshotStart {
            total_bytes: bytes.len() as u64,
        },
    )
    .await?;
    let mut chunks = 0_u64;
    for chunk in bytes.chunks(SNAPSHOT_CHUNK_BYTES) {
        send_response(
            sender,
            request_id,
            ServerResponse::SnapshotChunk {
                sequence: chunks,
                data: BASE64.encode(chunk),
            },
        )
        .await?;
        chunks += 1;
    }
    send_response(sender, request_id, ServerResponse::SnapshotEnd { chunks }).await
}

async fn respond_result<T>(
    sender: &mpsc::Sender<ServerFrame>,
    request_id: u64,
    result: Result<T, ProtocolFailure>,
    response: impl FnOnce(T) -> ServerResponse,
) -> Result<(), String> {
    match result {
        Ok(value) => send_response(sender, request_id, response(value)).await,
        Err(error) => send_failure(sender, request_id, error).await,
    }
}

async fn send_response(
    sender: &mpsc::Sender<ServerFrame>,
    request_id: u64,
    response: ServerResponse,
) -> Result<(), String> {
    let frame = ServerFrame {
        request_id,
        response,
    };
    let encoded = serde_json::to_vec(&frame).map_err(|error| error.to_string())?;
    if encoded.is_empty() || encoded.len() > MAX_FRAME_BYTES {
        let fallback = failure_frame(
            request_id,
            ProtocolFailure::new(
                ErrorCode::RequestTooLarge,
                "response cannot be represented within the negotiated frame bound",
            ),
        );
        sender
            .send(fallback)
            .await
            .map_err(|_| "response channel closed".to_owned())?;
        return Err("response exceeded the negotiated frame bound".to_owned());
    }
    sender
        .send(frame)
        .await
        .map_err(|_| "response channel closed".to_owned())
}

async fn send_failure(
    sender: &mpsc::Sender<ServerFrame>,
    request_id: u64,
    error: ProtocolFailure,
) -> Result<(), String> {
    send_response(
        sender,
        request_id,
        ServerResponse::Error {
            code: error.code,
            message: error.message,
            retryable: error.retryable,
        },
    )
    .await
}

fn failure_frame(request_id: u64, error: ProtocolFailure) -> ServerFrame {
    ServerFrame {
        request_id,
        response: ServerResponse::Error {
            code: error.code,
            message: error.message,
            retryable: error.retryable,
        },
    }
}

fn insert_active(
    active: &Mutex<BTreeMap<u64, CancellationToken>>,
    request_id: u64,
    token: CancellationToken,
) -> Result<(), String> {
    if active
        .lock()
        .map_err(|_| "active request lock is poisoned".to_owned())?
        .insert(request_id, token)
        .is_some()
    {
        return Err("request identifier is already active".to_owned());
    }
    Ok(())
}

fn remove_active(active: &Mutex<BTreeMap<u64, CancellationToken>>, request_id: u64) {
    if let Ok(mut active) = active.lock() {
        active.remove(&request_id);
    }
}

fn is_admin_request(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::DatabaseCreate { .. }
            | ClientRequest::DatabaseList
            | ClientRequest::DatabaseInspect { .. }
            | ClientRequest::DatabaseRename { .. }
            | ClientRequest::DatabaseDrop { .. }
            | ClientRequest::SnapshotExport { .. }
            | ClientRequest::SnapshotRestoreBegin { .. }
            | ClientRequest::SnapshotRestoreChunk { .. }
            | ClientRequest::SnapshotRestoreCommit
            | ClientRequest::SnapshotRestoreAbort
            | ClientRequest::LogicalExport { .. }
            | ClientRequest::LogicalImport { .. }
    )
}

fn changes_database_context(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::SelectDatabase { .. }
            | ClientRequest::DatabaseRename { .. }
            | ClientRequest::DatabaseDrop { .. }
            | ClientRequest::SnapshotRestoreBegin { .. }
            | ClientRequest::SnapshotRestoreChunk { .. }
            | ClientRequest::SnapshotRestoreCommit
            | ClientRequest::LogicalImport { .. }
    )
}

fn internal_failure(message: impl Into<String>) -> ProtocolFailure {
    ProtocolFailure::new(ErrorCode::InternalError, message)
}

async fn read_async_frame<R, T>(reader: &mut R) -> io::Result<T>
where
    R: AsyncRead + Unpin,
    T: for<'de> serde::Deserialize<'de>,
{
    let length = reader.read_u32().await? as usize;
    if length == 0 || length > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame size {length} is outside 1..={MAX_FRAME_BYTES}"),
        ));
    }
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload).await?;
    serde_json::from_slice(&payload)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

async fn write_async_frame<W, T>(writer: &mut W, value: &T) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: serde::Serialize,
{
    let payload = serde_json::to_vec(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if payload.is_empty() || payload.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "encoded frame size {} is outside 1..={MAX_FRAME_BYTES}",
                payload.len()
            ),
        ));
    }
    writer.write_u32(payload.len() as u32).await?;
    writer.write_all(&payload).await?;
    writer.flush().await
}
