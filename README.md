# nostdb-server

Source-available implementation repository for the installable single-node NostDB database daemon, licensed under SSPL-1.0.

Public-preview source only: no supported binary, hosted service, TLS endpoint, production SLA, or external contribution intake exists. Read [PREVIEW.md](PREVIEW.md), [SECURITY.md](SECURITY.md), and [CLA status](CLA.md).

The implemented product path is `nostd`: a long-running process that owns a versioned data directory and catalog, stores one or more named Databases, and accepts connections from the `nostdb` CLI and thin clients. It runs as the same binary in a foreground process, native operating-system service candidate, or Docker container with persistent config and data volumes. Read the [database server contract](docs/DATABASE_SERVER.md) and exact [database protocol](docs/DATABASE_PROTOCOL.md).

The old `nostdb-server` binary still opens one explicit `.ndb` and exposes HTTP protocol version 1 for current MCP compatibility. It is transitional, separately versioned, and must not be presented as the database product or as an application REST API platform.

## Package-manager targets

The intended global npm package installs the Server and matching CLI together; the Homebrew formula exposes the same two commands on macOS:

```bash
npm install --global @nostdb/server
# or
brew install nostdb/tap/nostdb

nostd --version
nostdb --version
```

Neither channel is published. The implemented `@nostdb/server` wrapper selects one exact native Server package, depends on the exact matching `@nostdb/cli`, contains no lifecycle downloader, and does not initialize or start the daemon during installation. CLI-only users install the separate `@nostdb/cli` package.

The Homebrew caveat keeps initialization explicit and first creates `~/.nostdb/data`, `config`, and `logs` with mode `0700`; run it as the service user without `sudo`.

## Initialize and run `nostd`

From this repository root, the following builds both sibling binaries and keeps all evaluation state in a disposable absolute temporary directory:

```bash
set -eu
WORKSPACE_ROOT="$(cd .. && pwd -P)"
EVALUATION_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/nostdb-server.XXXXXX")"
SERVER_BIN="$WORKSPACE_ROOT/nostdb-server/target/debug/nostd"
CLI_BIN="$WORKSPACE_ROOT/nostdb-cli/target/debug/nostdb"
DAEMON_PID=
cleanup() {
  if [ -n "${DAEMON_PID:-}" ]; then
    kill -TERM "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
    DAEMON_PID=
  fi
  rm -rf -- "$EVALUATION_ROOT"
}
trap cleanup EXIT

cargo build --locked --manifest-path "$WORKSPACE_ROOT/nostdb-server/Cargo.toml" --bin nostd
cargo build --locked --manifest-path "$WORKSPACE_ROOT/nostdb-cli/Cargo.toml" --bin nostdb
"$SERVER_BIN" init \
  --data-dir "$EVALUATION_ROOT/data" \
  --config "$EVALUATION_ROOT/server.toml" \
  --listen 127.0.0.1:7878
"$SERVER_BIN" serve --config "$EVALUATION_ROOT/server.toml" &
DAEMON_PID=$!

attempt=0
until "$CLI_BIN" server ping \
  --server nostdb://127.0.0.1:7878 \
  --credential-file "$EVALUATION_ROOT/data/credentials/client.token" >/dev/null 2>&1
do
  attempt=$((attempt + 1))
  test "$attempt" -lt 100
  sleep 0.05
done

"$CLI_BIN" database create knowledge \
  --server nostdb://127.0.0.1:7878 \
  --credential-file "$EVALUATION_ROOT/data/credentials/admin.token"
"$CLI_BIN" query \
  --server nostdb://127.0.0.1:7878 --database knowledge \
  --credential-file "$EVALUATION_ROOT/data/credentials/client.token" \
  'RETURN 1 AS ready'

cleanup
trap - EXIT
```

Initialization refuses to adopt a non-empty directory. It creates the versioned catalog plus separate protected `client.token` and `admin.token` files and prints only their paths. `serve` acquires the data-directory lock, recovers completed catalog/snapshot operations, opens every managed Database exclusively, and then accepts protocol connections.

The CLI also supports Database list/inspect/rename/guarded drop, physical snapshot/restore, logical export/import, and a remote REPL with transactions. It contains no HTTP endpoint knowledge.

## Data and deployment candidates

The managed tree uses immutable UUID Database IDs internally and never exposes paths to clients. Catalog writes and create/rename/drop operations are journaled. Snapshot restore validates a candidate through Core and uses a recovery journal plus rollback backup. Core sidecar locks prevent a second daemon, Embedded client, or Source synchronizer from opening the same `.ndb` while it is owned.

Candidate definitions are under [distribution](distribution/README.md):

- npm wrapper/platform candidates for `@nostdb/server`, exposing `nostd` and the exact matching `nostdb` CLI;
- Homebrew formula `nostdb`, commands `nostdb`/`nostd`, and a per-user service rooted at `~/.nostdb`;
- systemd service for `/etc/nostdb/server.toml` and `/var/lib/nostdb`;
- Windows foreground execution; Service Control Manager integration and protected credential ACL installation remain explicitly deferred;
- [Dockerfile](Dockerfile) and [compose.yaml](compose.yaml) with separate config and authoritative data volumes.

These are unpublished review candidates, not supported installers or registered services.

For the Compose candidate, initialize the named volumes exactly once before starting the server:

```bash
docker compose --profile init run --rm init
docker compose up server
```

Stop it with `docker compose down`. The named volumes intentionally remain for later starts; `docker compose down --volumes` also deletes all initialized database state.

## Transitional HTTP protocol version 1

Run the compatibility binary explicitly:

```bash
NOSTDB_API_KEY='replace-me' cargo run --bin nostdb-server -- \
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
| `GET`, `PUT /v1/admin/logical` | Export or validate and import a versioned `.nostdb` package |
| `GET /metrics` | Return operational counters |

Example query:

```bash
curl -sS http://127.0.0.1:8787/v1/query \
  -H 'Authorization: Bearer replace-me' \
  -H 'Content-Type: application/json' \
  --data '{"query":"MATCH (n) RETURN n","stream":true,"read_only":true}'
```

Snapshot restore opens and integrity-checks the uploaded Format 0 artifact before taking the live database lock or replacing the current file. Logical import uses a versioned package containing `nostdb.toml` plus normalized module paths and converts the synchronized candidate to Server/NDB authority explicitly.

## Verify

```bash
cargo metadata --format-version 1 --no-deps --locked
cargo fmt --all --check
cargo check --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps --locked
```

## License

Source-available under SSPL-1.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
