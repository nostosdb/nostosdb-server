use std::collections::BTreeMap;
use std::fs;
use std::net::TcpStream;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use nostdb_client::{
    Client, ClientError, ClientFrame, ClientRequest, ErrorCode, ServerFrame, ServerResponse,
    WireQueryLimits, read_frame, write_frame,
};
use nostdb_server::config::DaemonConfig;
use nostdb_server::daemon::DatabaseDaemon;
use serde_json::{Value, json};
use tokio::sync::oneshot;

struct Installation {
    root: PathBuf,
    config_path: PathBuf,
    admin: String,
    query: String,
}

impl Installation {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "nostdb-daemon-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir(&root).expect("test root creates");
        let config_path = root.join("server.toml");
        let report = DatabaseDaemon::initialize(&config_path, &root.join("data"), "127.0.0.1:0")
            .expect("daemon installation initializes");
        let admin = credential(&report.admin_credential_file);
        let query = credential(&report.query_credential_file);
        Self {
            root,
            config_path,
            admin,
            query,
        }
    }

    async fn start(&self, configure: impl FnOnce(&mut DaemonConfig)) -> RunningDaemon {
        let mut config = DaemonConfig::load(&self.config_path).expect("configuration loads");
        configure(&mut config);
        let daemon = DatabaseDaemon::open(config).expect("daemon opens");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener binds");
        let address = listener.local_addr().expect("listener address reads");
        let (shutdown, receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            nostdb_server::serve_database_protocol(listener, daemon, async {
                let _ = receiver.await;
            })
            .await
        });
        RunningDaemon {
            address: format!("nostdb://{address}"),
            shutdown: Some(shutdown),
            task,
        }
    }

    fn config(&self) -> DaemonConfig {
        DaemonConfig::load(&self.config_path).expect("configuration loads")
    }
}

impl Drop for Installation {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).expect("test installation removes");
    }
}

struct RunningDaemon {
    address: String,
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<Result<(), nostdb_server::ServerError>>,
}

impl RunningDaemon {
    async fn stop(mut self) {
        self.shutdown
            .take()
            .expect("shutdown sender exists")
            .send(())
            .ok();
        self.task
            .await
            .expect("daemon task joins")
            .expect("daemon stops cleanly");
    }
}

fn credential(path: &Path) -> String {
    fs::read_to_string(path)
        .expect("credential reads")
        .trim()
        .to_owned()
}

fn connect(address: &str, credential: &str) -> Client {
    Client::connect(address, credential, "database-protocol-test").expect("client connects")
}

fn request(client: &mut Client, request: ClientRequest) -> ServerResponse {
    client.request(request).expect("request succeeds")
}

fn select(client: &mut Client, database: &str) {
    assert!(matches!(
        request(
            client,
            ClientRequest::SelectDatabase {
                database: database.to_owned()
            }
        ),
        ServerResponse::DatabaseSelected { .. }
    ));
}

fn query(client: &mut Client, source: &str) -> Value {
    let response = request(
        client,
        ClientRequest::Query {
            query: source.to_owned(),
            parameters: BTreeMap::new(),
            read_only: false,
            stream: false,
            limits: None,
        },
    );
    let ServerResponse::Result { statement } = response else {
        panic!("query returned {response:?}")
    };
    statement
}

fn rows(statement: &Value) -> &[Value] {
    statement["result"]["rows"]
        .as_array()
        .expect("read rows exist")
}

fn server_error(result: Result<ServerResponse, ClientError>) -> ErrorCode {
    match result.expect_err("request must fail") {
        ClientError::Server { code, .. } => code,
        error => panic!("unexpected client error: {error}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn protocol_negotiates_authenticates_streams_transacts_cancels_and_limits() {
    let installation = Installation::new("protocol");

    #[cfg(unix)]
    for path in [
        &installation.config().authentication.query_credential_file,
        &installation.config().authentication.admin_credential_file,
    ] {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(path)
                .expect("metadata reads")
                .permissions()
                .mode()
                & 0o077,
            0
        );
    }

    let running = installation
        .start(|config| {
            config.limits.max_rows = 200_000;
            config.limits.max_operations = 100_000_000;
        })
        .await;

    let error = match Client::connect(&running.address, "x".repeat(64), "bad-client") {
        Ok(_) => panic!("invalid credential was accepted"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        ClientError::Server {
            code: ErrorCode::AuthenticationFailed,
            ..
        }
    ));

    let socket_address = running
        .address
        .strip_prefix("nostdb://")
        .expect("test address has scheme");
    let mut raw = TcpStream::connect(socket_address).expect("raw client connects");
    write_frame(
        &mut raw,
        &ClientFrame {
            request_id: 1,
            request: ClientRequest::Hello {
                protocol_version: 999,
                credential: installation.admin.clone(),
                client_name: "mismatch".to_owned(),
            },
        },
    )
    .expect("mismatch writes");
    let mismatch: ServerFrame = read_frame(&mut raw).expect("mismatch response reads");
    assert!(matches!(
        mismatch.response,
        ServerResponse::Error {
            code: ErrorCode::UnsupportedProtocol,
            ..
        }
    ));

    let mut ordinary = connect(&running.address, &installation.query);
    assert_eq!(
        server_error(ordinary.request(ClientRequest::DatabaseList)),
        ErrorCode::PermissionDenied
    );

    let mut admin = connect(&running.address, &installation.admin);
    assert!(matches!(
        request(
            &mut admin,
            ClientRequest::DatabaseCreate {
                name: "knowledge".to_owned()
            }
        ),
        ServerResponse::DatabaseCreated { .. }
    ));
    assert_eq!(
        server_error(admin.request(ClientRequest::DatabaseCreate {
            name: "knowledge".to_owned(),
        })),
        ErrorCode::DatabaseAlreadyExists
    );
    assert_eq!(
        server_error(admin.request(ClientRequest::Query {
            query: "RETURN 1".to_owned(),
            parameters: BTreeMap::new(),
            read_only: false,
            stream: false,
            limits: None,
        })),
        ErrorCode::DatabaseNotSelected
    );
    select(&mut admin, "knowledge");

    let mut parameters = BTreeMap::new();
    parameters.insert("values".to_owned(), json!([3, 1, 2]));
    let stream_id = admin
        .send(ClientRequest::Query {
            query: "UNWIND $values AS value RETURN value ORDER BY value".to_owned(),
            parameters,
            read_only: true,
            stream: true,
            limits: None,
        })
        .expect("stream request writes");
    assert!(matches!(
        admin.read().expect("stream start reads"),
        ServerFrame {
            request_id,
            response: ServerResponse::StreamStart { .. }
        } if request_id == stream_id
    ));
    let mut streamed = Vec::new();
    loop {
        let frame = admin.read().expect("stream frame reads");
        assert_eq!(frame.request_id, stream_id);
        match frame.response {
            ServerResponse::StreamRow { row } => streamed.push(row),
            ServerResponse::StreamEnd { rows } => {
                assert_eq!(rows, 3);
                break;
            }
            response => panic!("unexpected stream response {response:?}"),
        }
    }
    assert_eq!(streamed, [vec![json!(1)], vec![json!(2)], vec![json!(3)]]);

    assert!(matches!(
        request(&mut admin, ClientRequest::Begin),
        ServerResponse::Transaction { ref state, .. } if state == "begun"
    ));
    for name in ["Alice", "Bob"] {
        assert!(matches!(
            request(
                &mut admin,
                ClientRequest::Query {
                    query: format!("CREATE (n {{name: '{name}'}})"),
                    parameters: BTreeMap::new(),
                    read_only: false,
                    stream: false,
                    limits: None,
                }
            ),
            ServerResponse::Queued { .. }
        ));
    }
    assert!(matches!(
        request(&mut admin, ClientRequest::Commit),
        ServerResponse::Transaction { ref state, ref results } if state == "committed" && results.len() == 2
    ));
    assert_eq!(
        rows(&query(&mut admin, "MATCH (n) RETURN count(n) AS count")),
        &[json!([2])]
    );

    request(&mut admin, ClientRequest::Begin);
    request(
        &mut admin,
        ClientRequest::Query {
            query: "CREATE (n {name: 'Discarded'})".to_owned(),
            parameters: BTreeMap::new(),
            read_only: false,
            stream: false,
            limits: None,
        },
    );
    request(&mut admin, ClientRequest::Rollback);
    assert_eq!(
        rows(&query(&mut admin, "MATCH (n) RETURN count(n) AS count")),
        &[json!([2])]
    );

    request(&mut admin, ClientRequest::Begin);
    request(
        &mut admin,
        ClientRequest::Query {
            query: "CREATE (n {name: 'AtomicRollback'})".to_owned(),
            parameters: BTreeMap::new(),
            read_only: false,
            stream: false,
            limits: None,
        },
    );
    request(
        &mut admin,
        ClientRequest::Query {
            query: "NOT CYPHER".to_owned(),
            parameters: BTreeMap::new(),
            read_only: false,
            stream: false,
            limits: None,
        },
    );
    assert_eq!(
        server_error(admin.request(ClientRequest::Commit)),
        ErrorCode::QueryError
    );
    assert_eq!(
        rows(&query(&mut admin, "MATCH (n) RETURN count(n) AS count")),
        &[json!([2])]
    );

    for limits in [
        WireQueryLimits {
            max_rows: Some(0),
            max_memory_bytes: None,
            max_operations: None,
            max_traversals: None,
        },
        WireQueryLimits {
            max_rows: None,
            max_memory_bytes: Some(0),
            max_operations: None,
            max_traversals: None,
        },
        WireQueryLimits {
            max_rows: None,
            max_memory_bytes: None,
            max_operations: Some(0),
            max_traversals: None,
        },
    ] {
        assert_eq!(
            server_error(admin.request(ClientRequest::Query {
                query: "RETURN 'bounded' AS value".to_owned(),
                parameters: BTreeMap::new(),
                read_only: true,
                stream: false,
                limits: Some(limits),
            })),
            ErrorCode::ResourceLimit
        );
    }
    query(
        &mut admin,
        "MATCH (a {name: 'Alice'}), (b {name: 'Bob'}) CREATE (a)-[r]->(b)",
    );
    assert_eq!(
        server_error(admin.request(ClientRequest::Query {
            query: "MATCH (a)-[r]->(b) RETURN r".to_owned(),
            parameters: BTreeMap::new(),
            read_only: true,
            stream: false,
            limits: Some(WireQueryLimits {
                max_rows: None,
                max_memory_bytes: None,
                max_operations: None,
                max_traversals: Some(0),
            }),
        })),
        ErrorCode::ResourceLimit
    );

    let mut cancellation = connect(&running.address, &installation.admin);
    select(&mut cancellation, "knowledge");
    let values = (0..100_000).map(Value::from).collect::<Vec<_>>();
    let mut parameters = BTreeMap::new();
    parameters.insert("values".to_owned(), Value::Array(values));
    let target = cancellation
        .send(ClientRequest::Query {
            query: "UNWIND $values AS value RETURN value".to_owned(),
            parameters,
            read_only: true,
            stream: false,
            limits: None,
        })
        .expect("cancellable query writes");
    let cancel = cancellation
        .send(ClientRequest::Cancel {
            target_request_id: target,
        })
        .expect("cancel writes");
    let mut saw_ack = false;
    let mut saw_cancelled = false;
    for _ in 0..2 {
        let frame = cancellation.read().expect("cancellation response reads");
        match frame {
            ServerFrame {
                request_id,
                response: ServerResponse::Cancelled { target_request_id },
            } if request_id == cancel && target_request_id == target => saw_ack = true,
            ServerFrame {
                request_id,
                response:
                    ServerResponse::Error {
                        code: ErrorCode::Cancelled,
                        ..
                    },
            } if request_id == target => saw_cancelled = true,
            frame => panic!("unexpected cancellation frame {frame:?}"),
        }
    }
    assert!(saw_ack && saw_cancelled);

    running.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn protocol_timeout_never_reports_a_rolled_back_write_as_committed() {
    let installation = Installation::new("timeout");
    let running = installation
        .start(|config| config.limits.query_timeout_ms = 1)
        .await;
    let mut admin = connect(&running.address, &installation.admin);
    request(
        &mut admin,
        ClientRequest::DatabaseCreate {
            name: "timed".to_owned(),
        },
    );
    select(&mut admin, "timed");
    request(&mut admin, ClientRequest::Begin);
    for index in 0..1000 {
        assert!(matches!(
            request(
                &mut admin,
                ClientRequest::Query {
                    query: format!("CREATE (n {{index: {index}}})"),
                    parameters: BTreeMap::new(),
                    read_only: false,
                    stream: false,
                    limits: None,
                }
            ),
            ServerResponse::Queued { .. }
        ));
    }
    assert_eq!(
        server_error(admin.request(ClientRequest::Commit)),
        ErrorCode::ResourceLimit
    );
    assert_eq!(
        rows(&query(&mut admin, "MATCH (n) RETURN count(n) AS count")),
        &[json!([0])]
    );
    running.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn selected_database_stays_bound_to_its_stable_id_across_rename_and_name_reuse() {
    let installation = Installation::new("stable-selection");
    let running = installation.start(|_| {}).await;
    let mut admin = connect(&running.address, &installation.admin);
    let original = match request(
        &mut admin,
        ClientRequest::DatabaseCreate {
            name: "alpha".to_owned(),
        },
    ) {
        ServerResponse::DatabaseCreated { database } => database,
        response => panic!("unexpected create response {response:?}"),
    };
    let mut selected = connect(&running.address, &installation.query);
    select(&mut selected, "alpha");

    let renamed = match request(
        &mut admin,
        ClientRequest::DatabaseRename {
            database: "alpha".to_owned(),
            new_name: "beta".to_owned(),
        },
    ) {
        ServerResponse::DatabaseRenamed { database } => database,
        response => panic!("unexpected rename response {response:?}"),
    };
    assert_eq!(renamed.id, original.id);
    let replacement = match request(
        &mut admin,
        ClientRequest::DatabaseCreate {
            name: "alpha".to_owned(),
        },
    ) {
        ServerResponse::DatabaseCreated { database } => database,
        response => panic!("unexpected replacement create response {response:?}"),
    };
    assert_ne!(replacement.id, original.id);

    query(&mut selected, "CREATE (n {owner: 'original'})");
    select(&mut admin, "beta");
    assert_eq!(
        rows(&query(&mut admin, "MATCH (n) RETURN n.owner AS owner")),
        &[json!(["original"])]
    );
    select(&mut admin, "alpha");
    assert_eq!(
        rows(&query(&mut admin, "MATCH (n) RETURN count(n) AS count")),
        &[json!([0])]
    );

    request(
        &mut admin,
        ClientRequest::DatabaseDrop {
            database: "beta".to_owned(),
            confirm_name: "beta".to_owned(),
        },
    );
    assert_eq!(
        server_error(selected.request(ClientRequest::Query {
            query: "RETURN 1".to_owned(),
            parameters: BTreeMap::new(),
            read_only: true,
            stream: false,
            limits: None,
        })),
        ErrorCode::DatabaseNotFound
    );
    request(
        &mut admin,
        ClientRequest::DatabaseCreate {
            name: "beta".to_owned(),
        },
    );
    assert_eq!(
        server_error(selected.request(ClientRequest::Query {
            query: "RETURN 1".to_owned(),
            parameters: BTreeMap::new(),
            read_only: true,
            stream: false,
            limits: None,
        })),
        ErrorCode::DatabaseNotFound
    );
    running.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn named_databases_survive_restart_snapshot_logical_import_and_exclusive_ownership() {
    let installation = Installation::new("lifecycle");
    let running = installation.start(|_| {}).await;
    let mut admin = connect(&running.address, &installation.admin);
    let first = match request(
        &mut admin,
        ClientRequest::DatabaseCreate {
            name: "first".to_owned(),
        },
    ) {
        ServerResponse::DatabaseCreated { database } => database,
        response => panic!("unexpected create response {response:?}"),
    };
    request(
        &mut admin,
        ClientRequest::DatabaseCreate {
            name: "second".to_owned(),
        },
    );

    select(&mut admin, "first");
    query(&mut admin, "CREATE (n {name: 'First'})");
    select(&mut admin, "second");
    query(&mut admin, "CREATE (n {name: 'Second'})");

    let address = running.address.clone();
    let credential = installation.query.clone();
    let first_reader = std::thread::spawn({
        let address = address.clone();
        let credential = credential.clone();
        move || {
            let mut client = connect(&address, &credential);
            select(&mut client, "first");
            query(&mut client, "MATCH (n) RETURN n.name AS name")
        }
    });
    let second_reader = std::thread::spawn(move || {
        let mut client = connect(&address, &credential);
        select(&mut client, "second");
        query(&mut client, "MATCH (n) RETURN n.name AS name")
    });
    assert_eq!(
        rows(&first_reader.join().expect("first reader joins")),
        &[json!(["First"])]
    );
    assert_eq!(
        rows(&second_reader.join().expect("second reader joins")),
        &[json!(["Second"])]
    );

    let managed_path = installation
        .config()
        .data_directory
        .join("databases")
        .join(&first.id)
        .join("database.ndb");
    let direct = match nostdb_engine::EmbeddedDatabase::open(&managed_path) {
        Ok(_) => panic!("Embedded Mode opened daemon-owned storage"),
        Err(error) => error,
    };
    assert!(direct.to_string().contains("already owned"));
    let second_daemon = match DatabaseDaemon::open(installation.config()) {
        Ok(_) => panic!("second daemon owned the same data directory"),
        Err(error) => error,
    };
    assert!(second_daemon.to_string().contains("already owned"));

    let snapshot = export_snapshot(&mut admin, "first");
    select(&mut admin, "first");
    query(&mut admin, "CREATE (n {name: 'Later'})");
    restore_snapshot(&mut admin, "first", &snapshot);
    assert_eq!(
        rows(&query(
            &mut admin,
            "MATCH (n) RETURN n.name AS name ORDER BY name"
        )),
        &[json!(["First"])]
    );

    let logical = match request(
        &mut admin,
        ClientRequest::LogicalExport {
            database: "first".to_owned(),
        },
    ) {
        ServerResponse::LogicalPackage { package } => package,
        response => panic!("unexpected logical export {response:?}"),
    };
    assert!(matches!(
        request(
            &mut admin,
            ClientRequest::LogicalImport {
                database: "second".to_owned(),
                package: logical,
            }
        ),
        ServerResponse::LogicalImported { .. }
    ));
    select(&mut admin, "second");
    assert_eq!(
        rows(&query(
            &mut admin,
            "MATCH (n) RETURN n.name AS name ORDER BY name"
        )),
        &[json!(["First"])]
    );

    running.stop().await;
    let restarted = installation.start(|_| {}).await;
    let mut admin = connect(&restarted.address, &installation.admin);
    for database in ["first", "second"] {
        select(&mut admin, database);
        assert_eq!(
            rows(&query(&mut admin, "MATCH (n) RETURN n.name AS name")),
            &[json!(["First"])]
        );
    }
    let renamed = match request(
        &mut admin,
        ClientRequest::DatabaseRename {
            database: "first".to_owned(),
            new_name: "renamed".to_owned(),
        },
    ) {
        ServerResponse::DatabaseRenamed { database } => database,
        response => panic!("unexpected rename response {response:?}"),
    };
    assert_eq!(renamed.id, first.id);
    assert!(matches!(
        request(
            &mut admin,
            ClientRequest::DatabaseDrop {
                database: "renamed".to_owned(),
                confirm_name: "renamed".to_owned(),
            }
        ),
        ServerResponse::DatabaseDropped { .. }
    ));
    restarted.stop().await;
}

fn export_snapshot(client: &mut Client, database: &str) -> Vec<u8> {
    let id = client
        .send(ClientRequest::SnapshotExport {
            database: database.to_owned(),
        })
        .expect("snapshot request writes");
    let start = client.read().expect("snapshot start reads");
    assert_eq!(start.request_id, id);
    let ServerResponse::SnapshotStart { total_bytes } = start.response else {
        panic!("snapshot did not start")
    };
    let mut bytes = Vec::new();
    let mut expected = 0_u64;
    loop {
        let frame = client.read().expect("snapshot frame reads");
        assert_eq!(frame.request_id, id);
        match frame.response {
            ServerResponse::SnapshotChunk { sequence, data } => {
                assert_eq!(sequence, expected);
                expected += 1;
                bytes.extend(BASE64.decode(data).expect("snapshot base64 decodes"));
            }
            ServerResponse::SnapshotEnd { chunks } => {
                assert_eq!(chunks, expected);
                break;
            }
            response => panic!("unexpected snapshot response {response:?}"),
        }
    }
    assert_eq!(bytes.len() as u64, total_bytes);
    bytes
}

fn restore_snapshot(client: &mut Client, database: &str, bytes: &[u8]) {
    request(
        client,
        ClientRequest::SnapshotRestoreBegin {
            database: database.to_owned(),
            total_bytes: bytes.len() as u64,
        },
    );
    for (sequence, chunk) in bytes
        .chunks(nostdb_client::SNAPSHOT_CHUNK_BYTES)
        .enumerate()
    {
        request(
            client,
            ClientRequest::SnapshotRestoreChunk {
                sequence: sequence as u64,
                data: BASE64.encode(chunk),
            },
        );
    }
    assert!(matches!(
        request(client, ClientRequest::SnapshotRestoreCommit),
        ServerResponse::SnapshotRestore { ref state, .. } if state == "restored"
    ));
}
