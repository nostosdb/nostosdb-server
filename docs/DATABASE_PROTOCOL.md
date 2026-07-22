# NostosDB database protocol version 1

Status: implemented preview protocol; compatibility is governed independently from the HTTP compatibility adapter, `.nostos`, `.ndb`, catalog, and package versions.

## Transport and framing

Version 1 uses an ordered TCP byte stream. A client address has the form `nostos://IP:PORT`; version 1 provides no TLS negotiation. `nostosd init` binds to loopback by default. Exposing plaintext TCP beyond a trusted local or container boundary requires an explicit operator decision and is not presented as production-safe.

Every frame is exactly:

```text
4-byte unsigned big-endian JSON length
that many UTF-8 JSON bytes
```

The JSON length is in `1..=1048576`. A zero, oversized, truncated, non-UTF-8, or invalid JSON payload is a framing violation and the server closes the connection. Snapshot payloads use standard padded base64 chunks with at most 262144 decoded bytes so one frame remains bounded.

Client and server frames both contain a positive connection-local `request_id`. After `hello`, client request IDs must increase strictly. Every server event copies the initiating request ID. Several requests may be active concurrently, so events for different IDs may interleave; events for one streaming response retain their defined order.

The server places completed result events in a bounded queue and awaits each socket write. `stream_row` therefore inherits TCP and queue backpressure. Core currently materializes a result within the configured row and memory limits before the adapter emits rows; the protocol does not promise an unbounded cursor.

## Negotiation and authentication

The first client frame must be:

```json
{
  "request_id": 1,
  "type": "hello",
  "protocol_version": 1,
  "credential": "opaque-token",
  "client_name": "nostos-cli"
}
```

The server either returns one `hello` event or one typed error and closes the connection. It never falls back to a different version.

```json
{
  "request_id": 1,
  "type": "hello",
  "protocol_version": 1,
  "server_version": "0.0.1",
  "role": "admin",
  "max_frame_bytes": 1048576
}
```

Roles are:

- `query`: authenticated ping, Database selection, queries, streaming, transactions, and cancellation;
- `admin`: every query capability plus catalog lifecycle, physical snapshot, and logical package operations.

`nostosd init` writes distinct protected query and admin credential files. Credentials are not accepted as daemon or CLI command-line values. The CLI reads `NOSTOS_CREDENTIAL` or `--credential-file PATH`.

## Connection state

After authentication a connection has no selected Database, no transaction, and no snapshot upload.

1. `select_database` selects a catalog name and returns `database_selected` with stable ID, name, and state.
2. `query`, `begin`, `commit`, and `rollback` require a selected Database.
3. `begin` creates one connection-local transaction queue. Queries return `queued`; `commit` executes the complete queue through `nostos-engine::execute_transaction_limited`, and `rollback` discards it.
4. Database selection, rename/drop, restore, and logical import are rejected while a transaction is active.
5. Connection close cancels active cooperative queries, discards a queued transaction, and drops an incomplete snapshot upload.

The selected name is a catalog lookup, not a path. Managed file paths never appear in protocol frames.

## Query requests and results

`query` fields are:

```json
{
  "request_id": 4,
  "type": "query",
  "query": "UNWIND $values AS value RETURN value ORDER BY value",
  "parameters": {"values": [3, 1, 2]},
  "read_only": true,
  "stream": true,
  "limits": {"max_rows": 100}
}
```

Parameters accept JSON null, Boolean, finite number, string, list, and string-keyed map values. The adapter converts them to Core `QueryValue` values. Client limits may only lower the configured `max_rows`, `max_memory_bytes`, `max_operations`, and `max_traversals`; omitted values use server defaults. Explicit transaction queues use one server transaction budget.

`read_only: true` rejects mutating syntax before execution. Unsupported openCypher syntax remains an explicit Core diagnostic and is never reinterpreted.

A non-streaming success is `result` with the existing stable statement JSON shape:

```json
{
  "request_id": 4,
  "type": "result",
  "statement": {
    "kind": "read",
    "result": {"columns": ["value"], "rows": [[1]], "ordered": true}
  }
}
```

A streamed read is exactly:

```text
stream_start(columns, ordered)
zero or more stream_row(row)
stream_end(rows)
```

A write result remains one `result` event so its mutation summary and optional returned table are atomic. If any event cannot fit the negotiated frame, the request returns `request_too_large` rather than sending a partial oversized frame.

## Transactions and cancellation

`begin` returns `transaction` state `begun`. Each query returns `queued` with the queue length. `commit` returns `transaction` state `committed` plus statement results only after Core commits the whole batch. An execution error rolls back the complete batch. `rollback` returns state `rolled_back`.

`cancel` contains `target_request_id`. If that request is active on the same connection, the server signals Core's cooperative `CancellationToken` and returns `cancelled` for the cancel request. The target then terminates with error code `cancelled`. An unknown or completed target is a `protocol_violation`. Wall-clock timeout uses the same cooperative rollback path but reports `resource_limit`; if a write completed and committed at the timeout race, the server reports the committed success and never calls it a timeout.

## Database administration

All operations below require `admin`:

| Request | Result | Guard |
|---|---|---|
| `database_create(name)` | `database_created` | name must match `[a-z][a-z0-9_-]{0,62}` and be unique |
| `database_list` | `database_list` | deterministic name order; no paths |
| `database_inspect(database)` | `database_info` | format, generation, checksum, health, and counts |
| `database_rename(database,new_name)` | `database_renamed` | stable Database ID is unchanged |
| `database_drop(database,confirm_name)` | `database_dropped` | confirmation must exactly equal the current name |
| `logical_export(database)` | `logical_package` | portable versioned `.nostos` package |
| `logical_import(database,package)` | `logical_imported` | isolated Core compile, validate, then replace |

Create, rename, and drop use the versioned catalog lifecycle journal. A drop moves closed storage out of the active tree only after the catalog transition. Startup completes or rolls back a journal before accepting connections.

## Physical snapshots

`snapshot_export(database)` returns:

```text
snapshot_start(total_bytes)
snapshot_chunk(sequence=0,data=base64)
...
snapshot_end(chunks)
```

Restore is:

```text
snapshot_restore_begin(database,total_bytes) -> snapshot_restore(state="ready")
snapshot_restore_chunk(sequence,data)         -> snapshot_restore(state="chunk_accepted")
...
snapshot_restore_commit                      -> snapshot_restore(state="restored")
```

The upload must equal its declared size and remain within `max_snapshot_bytes`. The daemon writes a candidate, opens it through Core, runs integrity checks, adopts Server authority, checkpoints it, journals the replacement, and atomically swaps it with a rollback backup. `snapshot_restore_abort` discards the connection-local upload. Startup deterministically finishes or rolls back an interrupted replacement before opening the Database.

Physical restore is exact-format replacement. Logical import is portable source compilation. Neither operation silently merges with live data.

## Stable errors

Every operational failure is:

```json
{
  "request_id": 8,
  "type": "error",
  "code": "database_not_found",
  "message": "Database `missing` does not exist",
  "retryable": false
}
```

Version 1 codes are:

- `unsupported_protocol`, `authentication_failed`, `protocol_violation`, `permission_denied`;
- `database_not_selected`, `database_not_found`, `database_already_exists`, `invalid_database_name`, `database_busy`;
- `query_error`, `resource_limit`, `cancelled`;
- `transaction_already_active`, `no_transaction`;
- `request_too_large`, `snapshot_incompatible`, `recovery_required`, `internal_error`.

Errors do not expose credential values or managed filesystem paths. `retryable` is true only when unchanged retry may succeed, such as temporary ownership or lifecycle contention.

## Compatibility boundary

Database protocol version 1 is independent from HTTP compatibility protocol 1. The legacy `nostos-server` binary and `/v1/*` HTTP routes remain only for the current MCP compatibility path. New CLI and driver behavior uses `nostos-client` and this protocol, and requires no HTTP endpoint knowledge.
