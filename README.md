# nostosdb-server

Source-available implementation repository for the installable single-node NostosDB database daemon, licensed under SSPL-1.0.

Public-preview source only: no supported binary, hosted service, TLS endpoint, production SLA, or external contribution intake exists. Read [PREVIEW.md](PREVIEW.md), [SECURITY.md](SECURITY.md), and [CLA status](CLA.md).

The implemented product path is `nostosd`: a long-running process that owns a versioned data directory and catalog, stores one or more named Databases, and accepts connections from the `nostos` CLI and thin clients. It runs as the same binary in a foreground process, native operating-system service candidate, or Docker container with persistent config and data volumes. Read the [database server contract](docs/DATABASE_SERVER.md) and exact [database protocol](docs/DATABASE_PROTOCOL.md).

The old `nostos-server` binary still opens one explicit `.ndb` and exposes HTTP protocol version 1 for current MCP compatibility. It is transitional, separately versioned, and must not be presented as the database product or as an application REST API platform.

## Initialize and run `nostosd`

```bash
cargo run --bin nostosd -- init \
  --data-dir ./evaluation/data \
  --config ./evaluation/server.toml \
  --listen 127.0.0.1:7878
cargo run --bin nostosd -- serve --config ./evaluation/server.toml
```

Initialization refuses to adopt a non-empty directory. It creates the versioned catalog plus separate protected `client.token` and `admin.token` files and prints only their paths. `serve` acquires the data-directory lock, recovers completed catalog/snapshot operations, opens every managed Database exclusively, and then accepts protocol connections.

From the sibling CLI repository:

```bash
../nostosdb-cli/target/debug/nostos database create knowledge \
  --server nostos://127.0.0.1:7878 \
  --credential-file ./evaluation/data/credentials/admin.token
../nostosdb-cli/target/debug/nostos query \
  --server nostos://127.0.0.1:7878 --database knowledge \
  --credential-file ./evaluation/data/credentials/client.token \
  'RETURN 1 AS ready'
```

The CLI also supports Database list/inspect/rename/guarded drop, physical snapshot/restore, logical export/import, and a remote REPL with transactions. It contains no HTTP endpoint knowledge.

## Data and deployment candidates

The managed tree uses immutable UUID Database IDs internally and never exposes paths to clients. Catalog writes and create/rename/drop operations are journaled. Snapshot restore validates a candidate through Core and uses a recovery journal plus rollback backup. Core sidecar locks prevent a second daemon, Embedded client, or Source synchronizer from opening the same `.ndb` while it is owned.

Candidate definitions are under [distribution](distribution/README.md):

- Homebrew formula and service for `${HOMEBREW_PREFIX}/etc/nostosdb` and `${HOMEBREW_PREFIX}/var/nostosdb`;
- systemd service for `/etc/nostosdb/server.toml` and `/var/lib/nostosdb`;
- explicit Windows Service registration for `%PROGRAMDATA%\NostosDB\server.toml`;
- [Dockerfile](Dockerfile) and [compose.yaml](compose.yaml) with separate config and authoritative data volumes.

These are unpublished review candidates, not supported installers or registered services.

## Transitional HTTP protocol version 1

Run the compatibility binary explicitly:

```bash
NOSTOS_API_KEY='replace-me' cargo run --bin nostos-server -- \
  --database ./compatibility.ndb \
  --listen 127.0.0.1:8787
```

`GET /healthz` is the only unauthenticated HTTP endpoint. Every other endpoint requires `Authorization: Bearer <key>`.

The complete request, response, session, limit, and import/export contract is in
[docs/HTTP_API.md](docs/HTTP_API.md).

| Method and path | Behavior |
|---|---|
| `POST /v1/query` | Execute a bounded statement; `stream: true` returns JSONL |
| `GET /v1/catalog`, `/v1/schema`, `/v1/unresolved` | Read bounded catalog metadata |
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
  --data '{"query":"MATCH (n) RETURN n","stream":true,"read_only":true}'
```

Snapshot restore opens and integrity-checks the uploaded Format 0 artifact before taking the live database lock or replacing the current file. Logical import uses a versioned package containing `nostos.toml` plus normalized module paths and converts the synchronized candidate to Server/NDB authority explicitly.

## Verify

```bash
cargo metadata --no-deps --locked --workspace
cargo fmt --all --check
cargo check --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps --locked
```

## License

Source-available under SSPL-1.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
