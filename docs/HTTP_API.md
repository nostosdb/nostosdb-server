# NostosDB HTTP API

This document defines server protocol version 1. The Server protocol version,
`.nostos` language version, and `.ndb` format version are independent.

## Authentication and errors

`GET /healthz` is public. Every other endpoint requires
`Authorization: Bearer <api-key>`.

Errors use this envelope:

```json
{
  "error": {
    "code": "resource_limit",
    "message": "query row limit exceeded"
  }
}
```

The stable version 1 error codes are `bad_request`, `unauthorized`,
`request_too_large`, `resource_limit`, `query_timeout`, `query_error`,
`database_error`, `session_limit`, `session_not_found`, `transaction_active`,
`transaction_statement_limit`, `no_transaction`, `incompatible_snapshot`,
`logical_export_failed`, `unsupported_logical_package`,
`invalid_logical_package`, and `internal_error`.

## Query

`POST /v1/query` accepts:

```json
{
  "query": "MATCH (n) RETURN n",
  "parameters": {},
  "stream": false,
  "limits": {
    "max_rows": 1000,
    "max_memory_bytes": 1048576,
    "max_operations": 100000,
    "max_traversals": 10000,
    "timeout_ms": 5000
  }
}
```

Every `limits` field is optional. A request may lower a configured server
limit but cannot raise it. Zero means no allowance, except `timeout_ms`, which
must be positive. The server cooperatively checks cancellation, operation,
traversal, intermediate-memory, and result-row budgets. A write that reaches a
limit or timeout is rolled back before the error response is returned.

The normal response is one JSON value containing either `columns` and `rows`,
or mutation counters. With `stream: true`, a read response uses
`application/x-ndjson`: the first line contains `columns`, each following line
contains one `row`. Version 1 bounds and materializes the Core result before
writing JSONL, so streaming changes transport framing rather than the query
memory model.

## Sessions and transactions

- `POST /v1/sessions` creates a session and returns its opaque `session_id`.
- `DELETE /v1/sessions/{id}` removes it.
- `POST /v1/sessions/{id}/begin` starts a transaction queue.
- `POST /v1/sessions/{id}/query` executes immediately when no transaction is
  active. During a transaction it validates and queues the statement.
- `POST /v1/sessions/{id}/commit` executes the complete queue in one Core
  transaction and returns the ordered statement results.
- `POST /v1/sessions/{id}/rollback` discards the queue.

Queued statements do not become visible within the session before commit.
Commit is all-or-nothing, and other clients can observe neither intermediate
writes nor a timed-out or limit-terminated transaction. Request-specific limit
overrides are not accepted for queued statements; the transaction uses one
configured cumulative budget.

## Administration

`GET /metrics` returns Prometheus text counters and the active-session gauge.
`GET /healthz` returns liveness and `protocol_version` without opening an
authenticated administration surface.

### Snapshot boundary

`GET /v1/admin/snapshot` returns the authoritative `.ndb` bytes.
`PUT /v1/admin/snapshot` accepts an `.ndb` snapshot up to the separately
configured snapshot-body limit. The server writes it to an isolated temporary
path, opens it through the public Engine compatibility gate, and verifies its
integrity before taking the live database lock. Only then does it replace and
reopen the live database. An incompatible or corrupt upload leaves the live
database untouched.

Snapshots are physical and format-specific. Version 1 accepts only the exact
`.ndb` format supported by the running Core.

### Logical boundary

`GET /v1/admin/logical` returns a versioned JSON package:

```json
{
  "package_version": 1,
  "language_version": 1,
  "config": "language_version = 1\n...",
  "modules": [
    {
      "path": "modules/00000000-0000-0000-0000-000000000001.nostos",
      "stable_module_id": "00000000-0000-0000-0000-000000000001",
      "source": "module ..."
    }
  ]
}
```

`PUT /v1/admin/logical` accepts the same shape. Paths must be normalized,
relative, and traversal-free; module IDs must match the source modules. The
server constructs and synchronizes a complete candidate project in isolation,
then explicitly changes that candidate from Source authority to Server/NDB
authority before installing it. Invalid configuration, sources, imports,
constraints, or unrepresentable logical export state are rejected without
changing the live database.

Logical packages are portable source representations. They are not `.ndb`
snapshots and do not promise preservation of physical internal IDs.
