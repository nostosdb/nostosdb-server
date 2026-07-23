# NostDB database server contract

Status: Implemented source-preview architecture; release, production hardening, and cross-platform service evidence remain gated.

## Product definition

`nostd` is an installable, long-running, single-node database server. It owns one data directory, stores and recovers one or more named NostDB databases, and accepts connections from the `nostdb` CLI, official drivers, and thin adapters such as MCP.

It is not an application REST API server. Applications do not integrate by opening `.ndb` files or by depending on resource-oriented HTTP endpoints. They connect through the versioned NostDB database protocol and select a logical Database name.

The repository is named `nostdb-server`; the daemon executable is `nostd` so service managers and operators can distinguish it from the `nostdb` client.

## Deployment modes

NostDB retains two local execution choices:

| Mode | Process owning `.ndb` | Client address | Intended use |
|---|---|---|---|
| Embedded | Current `nostdb` process | Filesystem path | Local scripts, Source Mode, single-process applications |
| Server | Long-running `nostd` | Server address plus Database name | Shared local service, containers, multiple clients, remote applications |

In Server Mode, only `nostd` may open or mutate the managed `.ndb` files. A CLI, driver, Skill, MCP adapter, backup tool, or application must use the database protocol or an explicit offline administration workflow while the daemon is stopped.

## Operator surface

The implemented command surface is:

```bash
nostd init --data-dir /var/lib/nostdb
nostd serve --config /etc/nostdb/server.toml

nostdb server ping --server nostdb://127.0.0.1:7878 \
  --credential-file /var/lib/nostdb/credentials/client.token
nostdb database create knowledge --server nostdb://127.0.0.1:7878 \
  --credential-file /var/lib/nostdb/credentials/admin.token
nostdb database list --server nostdb://127.0.0.1:7878 \
  --credential-file /var/lib/nostdb/credentials/client.token
nostdb query --server nostdb://127.0.0.1:7878 --database knowledge \
  --credential-file /var/lib/nostdb/credentials/client.token \
  'MATCH (n) RETURN n LIMIT 100'
```

The `/var/lib` example assumes the daemon service identity owns the data tree. For the systemd candidate, do not initialize as `root` and leave root-owned `0600` credentials; follow the service-user procedure under `distribution/systemd`.

The CLI remains the primary interactive and administrative client, analogous to `psql`. `nostd` does not include an interactive shell.

Commands that delete a Database, replace a snapshot, change authority, or rotate credentials require explicit operator intent and must not be inferred from project discovery.

## Data directory and catalog

One daemon owns one configured data directory. Its logical contents are:

```text
data-directory/
├─ server-state        # versioned daemon/catalog metadata
├─ databases/
│  ├─ <stable-database-id>/
│  │  ├─ database.ndb
│  │  └─ runtime sidecars while open
│  └─ ...
├─ snapshots/          # optional operator-managed staging
└─ locks/              # daemon ownership and recovery metadata
```

This tree describes ownership, not a stable on-disk layout. Internal filenames may change. A logical Database has an immutable stable ID and a unique user-facing name; neither its name nor filesystem path is permanent storage identity.

The catalog must be explicitly versioned and must atomically map names to stable IDs and database state. Startup refuses:

- a data directory owned by another live daemon;
- unknown future catalog or `.ndb` versions;
- inconsistent catalog/file mappings;
- Source Mode materializations adopted without an explicit import or authority transition;
- partial restore or migration state that cannot be recovered deterministically.

Creating, renaming, importing, restoring, or dropping a Database updates the catalog and storage through a journaled operation. The daemon must never leave a catalog entry pointing to an absent or partially replaced database.

## Configuration

Server configuration and stored catalog metadata are separate versioned formats. A representative target configuration is:

```toml
config_version = 1
data_directory = "/var/lib/nostdb"

[network]
listen = "127.0.0.1:7878"

[authentication]
query_credential_file = "/var/lib/nostdb/credentials/client.token"
admin_credential_file = "/var/lib/nostdb/credentials/admin.token"

[limits]
max_connections = 256
max_sessions = 1024
max_transaction_statements = 1000
query_timeout_ms = 30000
max_rows = 10000
max_memory_bytes = 67108864
max_operations = 10000000
max_traversals = 1000000
max_snapshot_bytes = 1073741824
```

Secrets are not stored inline in this file by default. Authentication material comes from a permission-restricted credential file, operating-system service facility, secret manager, or container secret. Command-line arguments must not encourage secrets in shell history.

The daemon binds to loopback by default. Non-loopback TCP requires an explicit configuration change and a production transport-security plan; the preview must not imply that plaintext remote exposure is safe.

## Database protocol

The public network surface is a stateful, versioned database connection protocol, not a collection of application REST resources. It must support:

- protocol negotiation and an explicit unsupported-version error;
- authentication before Database access;
- Database selection by logical name;
- query parameters and structured result metadata;
- bounded result streaming with backpressure;
- sessions and atomic transactions;
- cancellation and server-enforced resource limits;
- typed, stable error codes;
- administrative capabilities separated from ordinary query permission;
- liveness/readiness without exposing catalog or data.

The protocol version remains independent from the `.nostdb` language version, `.ndb` format version, server catalog version, and binary package version.

Exact version 1 framing, connection state, messages, roles, and errors are specified in [DATABASE_PROTOCOL.md](DATABASE_PROTOCOL.md). The current HTTP protocol version 1 is a transitional evaluation transport used by existing tests and MCP. It is not the product identity and creates no compatibility relationship with database protocol version 1. The legacy HTTP binary remains an optional compatibility adapter and does not define new database semantics.

All query classification, planning, execution, transactions, validation, and storage behavior continue to come from public `nostdb-engine` APIs. The daemon and protocol adapter must not implement a second query engine or `.ndb` writer.

## Database lifecycle

The daemon owns these lifecycle operations:

- create and list named Databases;
- report Database health, format, generation, checksum, and authority;
- rename a Database without changing its stable ID;
- import a logical `.nostdb` package through an isolated synchronization candidate;
- restore an exact-compatible physical snapshot after isolated integrity validation;
- create a consistent snapshot without exposing live mutable files;
- close, migrate, and reopen a Database with rollback on failure;
- explicitly drop a Database through a guarded administrative operation.

Physical snapshot restore and logical import remain different capabilities. A snapshot is format-specific replacement/restore; a logical package is portable graph content. Neither is a silent merge.

## Concurrency and recovery

`nostd` provides the process boundary for multiple clients. It must preserve:

- transaction isolation and all-or-nothing commit;
- cancellation and resource-limit rollback;
- no observation of partial Database lifecycle operations;
- bounded connection, session, transaction, request, and result resources;
- graceful shutdown that stops new work and resolves active work according to a documented deadline;
- crash recovery before accepting connections;
- consistent backup/snapshot generation while clients are active.

The first implementation may serialize physical write transactions, but that is an implementation constraint rather than permission to expose partial state.

## Installation and service operation

The package-manager command targets are explicit. npm `0.0.1` is published;
the Homebrew tap remains unpublished:

```bash
# Server plus matching CLI
npm install --global @nostdb/server

# Server plus CLI on macOS
brew install nostdb/tap/nostdb
```

Both install surfaces expose `nostd` and `nostdb`. The published
`@nostdb/server@0.0.1` package depends on exact `@nostdb/cli@0.0.1`; neither
path installs a separate `@nostdb/core` package. The npm wrapper and its six
OS/CPU packages retain both `latest` and `next`. Installation only places
launchers and native binaries; it does not initialize state or start a daemon.

The release target includes the same `nostd` binary for foreground, native-service, and container execution:

```text
Direct archive: nostdb, nostd, licenses, notices, checksums
Homebrew:       brew services start nostdb
Linux:          systemd unit invoking nostd serve
Windows:        foreground nostd serve (Service Control Manager host deferred)
Docker:         nostd serve with a mounted data volume
```

The initial release process may build candidate artifacts without installing a persistent service. Publication, service registration on a real host, image push, signing, and production credentials require separate authorization. This repository contains no GitHub Actions.

Target defaults keep platform conventions without making their paths part of Database identity:

| Environment | Configuration | Data directory | Service form |
|---|---|---|---|
| Homebrew macOS | `~/.nostdb/config/server.toml` | `~/.nostdb/data` | per-user `nostdb` Homebrew service; logs in `~/.nostdb/logs` |
| Linux system package | `/etc/nostdb/server.toml` | `/var/lib/nostdb` | systemd unit with a dedicated account |
| Windows source preview | `%PROGRAMDATA%\\NostDB\\server.toml` | `%PROGRAMDATA%\\NostDB\\data` | Foreground console process; Service Control Manager integration is not implemented |
| Docker | `/etc/nostdb/server.toml` | `/var/lib/nostdb` | foreground PID 1 with a named volume |
| Direct developer run | explicit `--config` | explicit initialized directory | foreground process |

Every default is overridable through explicit installation/configuration. The daemon records normalized paths only as local operational state; protocol clients see stable Database IDs and names.

`nostd` resolves an omitted `--config` in this order: `NOSTDB_CONFIG`, `NOSTDB_HOME/config/server.toml`, then the platform default above. The Homebrew service sets `NOSTDB_HOME` to the installing user's `~/.nostdb` and should be run without `sudo`. Homebrew's package and service name is `nostdb`; the executable names remain `nostdb` and `nostd`.

The Linux systemd candidate must be initialized once as its `nostdb` service account. The exact [systemd initialization procedure](../distribution/systemd/README.md) creates `/etc/nostdb` and `/var/lib/nostdb` with deliberate ownership, runs `nostd init` as `nostdb`, then makes only `server.toml` root-owned and group-readable. This preserves service access to the generated `0600` credentials while preventing the daemon from rewriting its configuration.

The Docker contract mounts separate configuration and authoritative data volumes. The unpublished local candidate is initialized once, then its default `nostd serve --config /etc/nostdb/server.toml` command runs as PID 1:

```bash
docker volume create nostdb-config
docker volume create nostdb-data
docker run --rm \
  -v nostdb-config:/etc/nostdb \
  -v nostdb-data:/var/lib/nostdb \
  <local-image> init \
  --data-dir /var/lib/nostdb \
  --config /etc/nostdb/server.toml \
  --listen 0.0.0.0:7878
docker run --name nostdb \
  -p 127.0.0.1:7878:7878 \
  -v nostdb-config:/etc/nostdb \
  -v nostdb-data:/var/lib/nostdb \
  <local-image>
```

The equivalent unpublished Compose candidate is `compose.yaml`. No image is currently published.

## Initial non-goals

- application-specific REST resources or business logic;
- a hosted control plane;
- clustering, replication, sharding, or automatic failover;
- PostgreSQL wire, SQL, Neo4j Bolt, or full openCypher compatibility claims;
- direct client access to managed `.ndb` files;
- arbitrary user-selected storage paths in network requests;
- online cross-version storage migration without a verified rollback artifact;
- treating a health or metrics endpoint as the database client protocol.

## Acceptance criteria

The database-daemon Stage is complete only when evidence proves:

1. A fresh native installation can initialize a data directory, start `nostd`, create a named Database, connect with `nostdb`, write/query it, restart the daemon, and observe the committed data.
2. A Docker candidate performs the same lifecycle with authoritative data on a mounted volume and preserves it across container replacement.
3. At least two named Databases remain isolated across concurrent clients, restart, snapshot, and restore operations.
4. Managed `.ndb` files are exclusively daemon-owned while running; direct or second-daemon opens fail safely.
5. Protocol negotiation, authentication, Database selection, queries, streaming, transactions, cancellation, limits, and typed errors have exact client/server integration tests.
6. Native service definitions run the same binary/config/data-directory contract and default to local-only access.
7. The existing HTTP protocol is either an explicitly optional compatibility adapter or removed after MCP/clients migrate; it is not documented as the primary product surface.
