use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use nostdb_server::{AppState, ServerConfig, router};
use serde_json::{Value, json};
use tower::ServiceExt;

static SEQUENCE: AtomicU64 = AtomicU64::new(1);
const API_KEY: &str = "test-secret-key";

struct TestServer {
    directory: PathBuf,
    database: PathBuf,
    app: Router,
}

impl TestServer {
    fn new(name: &str, configure: impl FnOnce(&mut ServerConfig)) -> Self {
        let directory = std::env::temp_dir().join(format!(
            "nostdb-server-{name}-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).expect("temporary directory creates");
        let database = directory.join("graph.ndb");
        let mut config = ServerConfig::new(database.clone(), API_KEY.to_owned());
        configure(&mut config);
        let state = AppState::new(config).expect("server state initializes");
        Self {
            directory,
            database,
            app: router(state),
        }
    }

    async fn request(&self, method: &str, path: &str, body: impl Into<Body>) -> TestResponse {
        request(self.app.clone(), method, path, body, true).await
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let app = std::mem::replace(&mut self.app, Router::new());
        drop(app);
        std::fs::remove_dir_all(&self.directory).expect("temporary directory removes");
    }
}

struct TestResponse {
    status: StatusCode,
    content_type: Option<String>,
    body: Vec<u8>,
}

impl TestResponse {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("response is JSON")
    }
}

async fn request(
    app: Router,
    method: &str,
    path: &str,
    body: impl Into<Body>,
    authenticated: bool,
) -> TestResponse {
    let mut builder = Request::builder().method(method).uri(path);
    if authenticated {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {API_KEY}"));
    }
    if method == "POST" {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    let response = app
        .oneshot(builder.body(body.into()).expect("request builds"))
        .await
        .expect("router responds");
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body reads")
        .to_vec();
    TestResponse {
        status,
        content_type,
        body,
    }
}

fn query_body(query: &str) -> Body {
    Body::from(serde_json::to_vec(&json!({"query": query})).expect("query request serializes"))
}

fn result_rows(value: &Value) -> &[Value] {
    value["result"]["rows"]
        .as_array()
        .expect("result rows exist")
}

#[tokio::test]
async fn health_is_public_but_every_operational_endpoint_requires_authentication() {
    let server = TestServer::new("auth", |_| {});
    let health = request(server.app.clone(), "GET", "/healthz", Body::empty(), false).await;
    assert_eq!(health.status, StatusCode::OK);
    assert_eq!(health.json()["status"], "ok");

    for path in ["/v1/query", "/v1/sessions", "/metrics"] {
        let response = request(
            server.app.clone(),
            if path == "/metrics" { "GET" } else { "POST" },
            path,
            query_body("RETURN 1"),
            false,
        )
        .await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(response.json()["error"]["code"], "unauthorized");
    }

    let metrics = server.request("GET", "/metrics", Body::empty()).await;
    assert_eq!(metrics.status, StatusCode::OK);
    assert!(
        String::from_utf8(metrics.body)
            .expect("metrics are UTF-8")
            .contains("nostdb_auth_failures_total 3")
    );
}

#[tokio::test]
async fn queries_support_json_parameters_and_jsonl_streaming() {
    let server = TestServer::new("query", |_| {});
    let request = json!({
        "query": "UNWIND $values AS value RETURN value ORDER BY value",
        "parameters": {"values": [3, 1, 2]},
        "stream": true,
    });
    let response = server
        .request(
            "POST",
            "/v1/query",
            serde_json::to_vec(&request).expect("request serializes"),
        )
        .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response.content_type.as_deref(),
        Some("application/x-ndjson")
    );
    let lines = String::from_utf8(response.body)
        .expect("stream is UTF-8")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("line is JSON"))
        .collect::<Vec<_>>();
    assert_eq!(lines[0]["columns"], json!(["value"]));
    assert_eq!(lines[1]["value"], 1);
    assert_eq!(lines[3]["value"], 3);
}

#[tokio::test]
async fn read_only_queries_and_catalog_endpoints_are_enforced_and_bounded() {
    let server = TestServer::new("mcp-read-only", |config| {
        config.query_limits.max_rows = 1;
    });
    let rejected = server
        .request(
            "POST",
            "/v1/query",
            Body::from(
                serde_json::to_vec(&json!({
                    "query": "CREATE (n {name: 'Forbidden'})",
                    "read_only": true,
                }))
                .expect("request serializes"),
            ),
        )
        .await;
    assert_eq!(rejected.status, StatusCode::BAD_REQUEST);
    assert_eq!(rejected.json()["error"]["code"], "query_error");

    let count = server
        .request(
            "POST",
            "/v1/query",
            Body::from(
                serde_json::to_vec(&json!({
                    "query": "MATCH (n) RETURN count(n) AS count",
                    "read_only": true,
                }))
                .expect("request serializes"),
            ),
        )
        .await;
    assert_eq!(count.status, StatusCode::OK);
    assert_eq!(result_rows(&count.json()), &[json!([0])]);

    let catalog = server.request("GET", "/v1/catalog", Body::empty()).await;
    assert_eq!(catalog.status, StatusCode::OK);
    assert_eq!(catalog.json()["protocol_version"], 1);
    assert_eq!(catalog.json()["counts"]["nodes"], 0);
    assert_eq!(
        catalog.json()["database"]["logical_checksum"]
            .as_str()
            .expect("checksum is text")
            .len(),
        16
    );

    let schema = server
        .request("GET", "/v1/schema?limit=999", Body::empty())
        .await;
    assert_eq!(schema.status, StatusCode::OK);
    assert_eq!(schema.json()["returned"], 0);
    assert_eq!(schema.json()["truncated"], false);

    let unresolved = server
        .request("GET", "/v1/unresolved?limit=999", Body::empty())
        .await;
    assert_eq!(unresolved.status, StatusCode::OK);
    assert_eq!(unresolved.json()["returned"], 0);
    assert_eq!(unresolved.json()["truncated"], false);

    let invalid = server
        .request("GET", "/v1/schema?limit=-1", Body::empty())
        .await;
    assert_eq!(invalid.status, StatusCode::BAD_REQUEST);
    assert_eq!(invalid.json()["error"]["code"], "bad_request");

    for path in ["/v1/catalog", "/v1/schema", "/v1/unresolved"] {
        let response = request(server.app.clone(), "GET", path, Body::empty(), false).await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "{path}");
    }
}

#[tokio::test]
async fn transaction_commit_is_atomic_for_concurrent_clients() {
    let server = TestServer::new("transaction", |_| {});
    let created = server.request("POST", "/v1/sessions", Body::empty()).await;
    assert_eq!(created.status, StatusCode::CREATED);
    let id = created.json()["session_id"]
        .as_str()
        .expect("session id exists")
        .to_owned();
    assert_eq!(
        server
            .request("POST", &format!("/v1/sessions/{id}/begin"), Body::empty())
            .await
            .status,
        StatusCode::OK
    );
    for name in ["Alice", "Bob"] {
        let queued = server
            .request(
                "POST",
                &format!("/v1/sessions/{id}/query"),
                query_body(&format!("CREATE (n {{name: '{name}'}})")),
            )
            .await;
        assert_eq!(queued.status, StatusCode::OK);
        assert_eq!(queued.json()["status"], "queued");
    }

    let before = server
        .request(
            "POST",
            "/v1/query",
            query_body("MATCH (n) RETURN n.name AS name ORDER BY name"),
        )
        .await;
    assert!(result_rows(&before.json()).is_empty());

    let commit_path = format!("/v1/sessions/{id}/commit");
    let commit_request = request(
        server.app.clone(),
        "POST",
        &commit_path,
        Body::empty(),
        true,
    );
    let concurrent_read = request(
        server.app.clone(),
        "POST",
        "/v1/query",
        query_body("MATCH (n) RETURN n.name AS name ORDER BY name"),
        true,
    );
    let (commit, observed) = tokio::join!(commit_request, concurrent_read);
    assert_eq!(commit.status, StatusCode::OK);
    let observed = observed.json();
    let observed_rows = result_rows(&observed);
    assert!(
        observed_rows.is_empty() || observed_rows.len() == 2,
        "a concurrent reader must see neither or both writes"
    );

    let after = server
        .request(
            "POST",
            "/v1/query",
            query_body("MATCH (n) RETURN n.name AS name ORDER BY name"),
        )
        .await;
    assert_eq!(
        result_rows(&after.json()),
        &[json!(["Alice"]), json!(["Bob"])]
    );

    let second = server.request("POST", "/v1/sessions", Body::empty()).await;
    let second_id = second.json()["session_id"]
        .as_str()
        .expect("session id exists")
        .to_owned();
    server
        .request(
            "POST",
            &format!("/v1/sessions/{second_id}/begin"),
            Body::empty(),
        )
        .await;
    server
        .request(
            "POST",
            &format!("/v1/sessions/{second_id}/query"),
            query_body("CREATE (n {name: 'Discarded'})"),
        )
        .await;
    let rolled_back = server
        .request(
            "POST",
            &format!("/v1/sessions/{second_id}/rollback"),
            Body::empty(),
        )
        .await;
    assert_eq!(rolled_back.status, StatusCode::OK);
    assert_names(&server, &["Alice", "Bob"]).await;
}

#[tokio::test]
async fn resource_limit_and_timeout_cancel_writes_before_commit() {
    let limited = TestServer::new("limits", |_| {});
    let request = json!({
        "query": "CREATE (n {name: 'Rejected'}) RETURN n",
        "limits": {"max_rows": 0},
    });
    let response = limited
        .request(
            "POST",
            "/v1/query",
            serde_json::to_vec(&request).expect("request serializes"),
        )
        .await;
    assert_eq!(response.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.json()["error"]["code"], "resource_limit");
    let read = limited
        .request("POST", "/v1/query", query_body("MATCH (n) RETURN n"))
        .await;
    assert!(result_rows(&read.json()).is_empty());

    for (query, limits) in [
        ("RETURN 1", json!({"max_operations": 0})),
        ("RETURN 'value'", json!({"max_memory_bytes": 0})),
    ] {
        let request = json!({"query": query, "limits": limits});
        let response = limited
            .request(
                "POST",
                "/v1/query",
                serde_json::to_vec(&request).expect("request serializes"),
            )
            .await;
        assert_eq!(response.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.json()["error"]["code"], "resource_limit");
    }

    for query in [
        "CREATE (a {name: 'Alice'})",
        "CREATE (b {name: 'Bob'})",
        "MATCH (a {name: 'Alice'}), (b {name: 'Bob'}) CREATE (a)-[r]->(b)",
    ] {
        assert_eq!(
            limited
                .request("POST", "/v1/query", query_body(query))
                .await
                .status,
            StatusCode::OK
        );
    }
    let traversal = json!({
        "query": "MATCH (a)-[r]->(b) RETURN r",
        "limits": {"max_traversals": 0},
    });
    let response = limited
        .request(
            "POST",
            "/v1/query",
            serde_json::to_vec(&traversal).expect("request serializes"),
        )
        .await;
    assert_eq!(response.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.json()["error"]["code"], "resource_limit");

    let timed = TestServer::new("timeout", |config| {
        config.query_timeout = Duration::from_nanos(1);
    });
    let mut committed = 0;
    let mut timed_out = 0;
    for index in 0..20 {
        let response = timed
            .request(
                "POST",
                "/v1/query",
                query_body(&format!("CREATE (n {{index: {index}}})")),
            )
            .await;
        match response.status {
            StatusCode::OK => committed += 1,
            StatusCode::REQUEST_TIMEOUT => timed_out += 1,
            status => panic!("unexpected timeout-race response: {status}"),
        }
    }
    assert!(timed_out > 0, "the nanosecond budget must cancel work");
    let catalog = timed.request("GET", "/v1/catalog", Body::empty()).await;
    assert_eq!(catalog.status, StatusCode::OK);
    assert_eq!(
        catalog.json()["counts"]["nodes"],
        json!(committed),
        "only requests reported as successful may commit"
    );

    let body_limited = TestServer::new("body-limit", |config| {
        config.request_body_bytes = 64;
        config.snapshot_body_bytes = 1024;
    });
    let oversized = body_limited
        .request(
            "POST",
            "/v1/query",
            Body::from(format!("{{\"query\":\"RETURN '{}'\"}}", "x".repeat(100))),
        )
        .await;
    assert_eq!(oversized.status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn snapshot_restore_validates_compatibility_before_replacing_live_database() {
    let server = TestServer::new("snapshot", |_| {});
    let create = server
        .request(
            "POST",
            "/v1/query",
            query_body("CREATE (n {name: 'Before'})"),
        )
        .await;
    assert_eq!(create.status, StatusCode::OK);
    let exported = server
        .request("GET", "/v1/admin/snapshot", Body::empty())
        .await;
    assert_eq!(exported.status, StatusCode::OK);
    assert_eq!(
        exported.content_type.as_deref(),
        Some("application/vnd.nostdb.ndb")
    );

    let incompatible = server
        .request("PUT", "/v1/admin/snapshot", b"not an ndb".to_vec())
        .await;
    assert_eq!(incompatible.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        incompatible.json()["error"]["code"],
        "incompatible_snapshot"
    );
    assert_names(&server, &["Before"]).await;

    server
        .request(
            "POST",
            "/v1/query",
            query_body("CREATE (n {name: 'After'})"),
        )
        .await;
    assert_names(&server, &["After", "Before"]).await;
    let restored = server
        .request("PUT", "/v1/admin/snapshot", exported.body)
        .await;
    assert_eq!(restored.status, StatusCode::OK);
    assert_names(&server, &["Before"]).await;

    assert!(Path::new(&server.database).is_file());
    let logical = server
        .request("GET", "/v1/admin/logical", Body::empty())
        .await;
    assert_eq!(logical.status, StatusCode::OK);
    let package = logical.json();
    assert_eq!(package["package_version"], 1);
    assert_eq!(package["language_version"], 1);
    assert!(package["modules"][0].get("stable_module_id").is_some());
    assert!(package["modules"][0].get("module_id").is_none());
    let mut unsupported_language = package.clone();
    unsupported_language["language_version"] = json!(2);
    let unsupported = server
        .request(
            "PUT",
            "/v1/admin/logical",
            serde_json::to_vec(&unsupported_language).expect("logical package serializes"),
        )
        .await;
    assert_eq!(unsupported.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        unsupported.json()["error"]["code"],
        "unsupported_logical_package"
    );
    assert_eq!(
        package["modules"].as_array().expect("modules exist").len(),
        1
    );
    server
        .request(
            "POST",
            "/v1/query",
            query_body("CREATE (n {name: 'Temporary'})"),
        )
        .await;
    assert_names(&server, &["Before", "Temporary"]).await;
    let imported = server
        .request(
            "PUT",
            "/v1/admin/logical",
            serde_json::to_vec(&package).expect("logical package serializes"),
        )
        .await;
    assert_eq!(
        imported.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&imported.body)
    );
    assert_names(&server, &["Before"]).await;

    let mut unsafe_package = package;
    unsafe_package["modules"][0]["path"] = json!("../outside.nostdb");
    let rejected = server
        .request(
            "PUT",
            "/v1/admin/logical",
            serde_json::to_vec(&unsafe_package).expect("logical package serializes"),
        )
        .await;
    assert_eq!(rejected.status, StatusCode::BAD_REQUEST);
    assert_names(&server, &["Before"]).await;
    assert!(
        std::fs::read_dir(&server.directory)
            .expect("server directory reads")
            .all(|entry| !entry
                .expect("server directory entry reads")
                .file_name()
                .to_string_lossy()
                .starts_with(".nostdb-logical-import-"))
    );
}

async fn assert_names(server: &TestServer, expected: &[&str]) {
    let response = server
        .request(
            "POST",
            "/v1/query",
            query_body("MATCH (n) RETURN n.name AS name ORDER BY name"),
        )
        .await;
    assert_eq!(response.status, StatusCode::OK);
    let expected = expected
        .iter()
        .map(|name| json!([name]))
        .collect::<Vec<_>>();
    assert_eq!(result_rows(&response.json()), expected);
}
