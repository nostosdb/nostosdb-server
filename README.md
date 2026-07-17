# nostos-server

Source-available single-node HTTP server for NostosDB, licensed under SSPL-1.0.

The server treats `.ndb` as authoritative and consumes only the public `nostos-engine` facade. It provides API-key authentication, JSON and JSONL query results, queued session transactions, cooperative time/row/memory/operation/traversal limits, health and Prometheus-style metrics, structured tracing, and separate logical `.nostos` package and snapshot `.ndb` import/export endpoints.

## Run

```bash
NOSTOS_API_KEY='replace-me' cargo run -- \
  --database ./server.ndb \
  --listen 127.0.0.1:7878
```

`GET /healthz` is the only unauthenticated endpoint. Every other endpoint requires `Authorization: Bearer <key>`.

## Protocol version 1

The complete request, response, session, limit, and import/export contract is in
[docs/HTTP_API.md](docs/HTTP_API.md).

| Method and path | Behavior |
|---|---|
| `POST /v1/query` | Execute a bounded statement; `stream: true` returns JSONL |
| `POST /v1/sessions` | Create a session |
| `POST /v1/sessions/{id}/begin` | Begin a queued atomic transaction |
| `POST /v1/sessions/{id}/query` | Execute immediately or queue in the active transaction |
| `POST /v1/sessions/{id}/commit` | Execute and commit the complete queue atomically |
| `POST /v1/sessions/{id}/rollback` | Discard the queue |
| `GET`, `PUT /v1/admin/snapshot` | Export or compatibility-check and restore `.ndb` |
| `GET`, `PUT /v1/admin/logical` | Export or validate and import a versioned `.nostos` package |
| `GET /metrics` | Return operational counters |

Example query:

```bash
curl -sS http://127.0.0.1:7878/v1/query \
  -H 'Authorization: Bearer replace-me' \
  -H 'Content-Type: application/json' \
  --data '{"query":"MATCH (n) RETURN n","stream":true}'
```

Snapshot restore opens and integrity-checks the uploaded Format 0 artifact before taking the live database lock or replacing the current file. Logical import uses a versioned package containing `nostos.toml` plus normalized module paths and converts the synchronized candidate to Server/NDB authority explicitly.

## Verify

```bash
cargo metadata --no-deps --locked
cargo fmt --all --check
cargo check --all-targets --all-features --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-features --locked
RUSTDOCFLAGS='-D warnings' cargo doc --all-features --no-deps --locked
```

## License

Source-available under SSPL-1.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
