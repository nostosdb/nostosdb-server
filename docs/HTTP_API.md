# NostDB HTTP API

Status: implemented transitional evaluation transport.

This document defines the currently implemented HTTP protocol version 1. It is retained for exact regression coverage and the current thin MCP adapter, but it is not the NostDB database connection protocol and must not be treated as an application REST API platform. The installable daemon contract is defined in [DATABASE_SERVER.md](DATABASE_SERVER.md), and new CLI/driver framing is defined in [DATABASE_PROTOCOL.md](DATABASE_PROTOCOL.md).

The HTTP protocol version,
`.nost` language version, and `.nostdb` format version are independent.

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
  "read_only": false,
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

`read_only: true` requires the statement to pass the Core read-only preparation
path before execution. Mutating clauses are rejected with `query_error`; the
server does not use text matching or silently remove unsupported syntax. This
flag is intended for replaceable read-only consumers such as MCP adapters.

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

### Read-only catalog

The authenticated `GET /v1/catalog` endpoint returns protocol, format,
generation, logical checksum, authority, and graph-count metadata.
`GET /v1/schema` and `GET /v1/unresolved` return deterministic arrays. Their
optional non-negative `limit` query parameter is capped by the configured
query row limit, and each response includes `returned` and `truncated` fields.
These endpoints expose no source text, snapshot bytes, or mutation operation.

### Snapshot boundary

`GET /v1/admin/snapshot` returns the authoritative `.nostdb` bytes.
`PUT /v1/admin/snapshot` accepts an `.nostdb` snapshot up to the separately
configured snapshot-body limit. The server writes it to an isolated temporary
path, opens it through the public Engine compatibility gate, and verifies its
integrity before taking the live database lock. Only then does it replace and
reopen the live database. An incompatible or corrupt upload leaves the live
database untouched.

Snapshots are physical and format-specific. Version 1 accepts only the exact
`.nostdb` format supported by the running Core.

### Logical boundary

`GET /v1/admin/logical` returns a versioned JSON package:

```json
{
  "package_version": 1,
  "language_version": 1,
  "settings": "{\"version\":1,\"database\":{\"root\":\"root.nostdb\",\"links\":[]},\"source\":{\"version\":1,\"enabled\":true,\"include\":[\"modules/*.nost\"],\"modules\":{\"modules/00000000-0000-0000-0000-000000000001.nost\":\"00000000-0000-0000-0000-000000000001\"}}}\n",
  "modules": [
    {
      "path": "modules/00000000-0000-0000-0000-000000000001.nost",
      "stable_module_id": "00000000-0000-0000-0000-000000000001",
      "source": "module ..."
    }
  ]
}
```

`PUT /v1/admin/logical` accepts the same shape. Paths must be normalized,
relative, and traversal-free; module IDs must match the source modules and
settings mappings. The server writes both below a candidate `.nostdb/`,
synchronizes the complete project in isolation, then explicitly changes that
candidate from Source authority to Server/NDB authority before installing it.
Invalid settings, sources, imports, constraints, or unrepresentable logical
export state are rejected without changing the live database.

Logical packages are portable source representations. They are not `.nostdb`
snapshots and do not promise preservation of physical internal IDs.
