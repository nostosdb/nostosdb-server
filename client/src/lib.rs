#![forbid(unsafe_code)]
#![deny(missing_docs)]
//! Blocking client and shared wire types for the NostDB database protocol.
//!
//! This crate intentionally contains no parser, query engine, or storage code.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The independently versioned database protocol implemented by this crate.
pub const DATABASE_PROTOCOL_VERSION: u32 = 1;
/// Maximum encoded JSON payload accepted in one frame.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
/// Recommended decoded payload size for one snapshot chunk.
pub const SNAPSHOT_CHUNK_BYTES: usize = 256 * 1024;

/// One client-to-server frame.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClientFrame {
    /// Connection-local request identifier.
    pub request_id: u64,
    /// Requested operation.
    #[serde(flatten)]
    pub request: ClientRequest,
}

/// One server-to-client frame.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServerFrame {
    /// Request identifier copied from the initiating client frame.
    pub request_id: u64,
    /// Result event or typed error.
    #[serde(flatten)]
    pub response: ServerResponse,
}

/// Client operations in database protocol version 1.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientRequest {
    /// Negotiates the protocol and authenticates the connection.
    Hello {
        /// Exact protocol version requested by the client.
        protocol_version: u32,
        /// Opaque credential read from an environment or protected file.
        credential: String,
        /// Diagnostic client name, never used for authorization.
        client_name: String,
    },
    /// Checks authenticated connection liveness.
    Ping,
    /// Selects the logical Database used by subsequent query operations.
    SelectDatabase {
        /// Unique catalog name, not a filesystem path.
        database: String,
    },
    /// Executes or queues one query.
    Query {
        /// Query text interpreted only by `nostdb-engine`.
        query: String,
        /// Named JSON-compatible query parameters.
        #[serde(default)]
        parameters: BTreeMap<String, Value>,
        /// Rejects write syntax when true.
        #[serde(default)]
        read_only: bool,
        /// Requests row events instead of one materialized result frame.
        #[serde(default)]
        stream: bool,
        /// Optional limits that may only lower server defaults.
        #[serde(default)]
        limits: Option<WireQueryLimits>,
    },
    /// Starts an explicit atomic transaction queue.
    Begin,
    /// Executes and commits the queued transaction atomically.
    Commit,
    /// Discards the queued transaction.
    Rollback,
    /// Requests cooperative cancellation of an active request on this connection.
    Cancel {
        /// Request identifier of the active query.
        target_request_id: u64,
    },
    /// Creates a managed logical Database.
    DatabaseCreate {
        /// New unique catalog name.
        name: String,
    },
    /// Lists managed Databases without exposing storage paths.
    DatabaseList,
    /// Inspects one managed Database.
    DatabaseInspect {
        /// Catalog name to inspect.
        database: String,
    },
    /// Renames a Database without changing its stable identity.
    DatabaseRename {
        /// Existing catalog name.
        database: String,
        /// New unique catalog name.
        new_name: String,
    },
    /// Drops a Database through an exact-name confirmation guard.
    DatabaseDrop {
        /// Existing catalog name.
        database: String,
        /// Must exactly equal `database`.
        confirm_name: String,
    },
    /// Exports a consistent physical snapshot as bounded chunk events.
    SnapshotExport {
        /// Catalog name to snapshot.
        database: String,
    },
    /// Begins a bounded physical snapshot restore upload.
    SnapshotRestoreBegin {
        /// Catalog name to replace after validation.
        database: String,
        /// Exact decoded byte count expected across chunks.
        total_bytes: u64,
    },
    /// Appends one base64-encoded snapshot upload chunk.
    SnapshotRestoreChunk {
        /// Zero-based contiguous chunk sequence.
        sequence: u64,
        /// Standard padded base64 payload.
        data: String,
    },
    /// Validates and atomically installs the uploaded snapshot.
    SnapshotRestoreCommit,
    /// Discards an incomplete snapshot upload.
    SnapshotRestoreAbort,
    /// Exports the portable logical `.nostdb` package.
    LogicalExport {
        /// Catalog name to export.
        database: String,
    },
    /// Imports a portable logical package after isolated compilation.
    LogicalImport {
        /// Catalog name to replace after validation.
        database: String,
        /// Versioned logical package document.
        package: Value,
    },
}

/// Server result events in database protocol version 1.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerResponse {
    /// Successful protocol negotiation and authentication.
    Hello {
        /// Negotiated database protocol version.
        protocol_version: u32,
        /// Server binary package version.
        server_version: String,
        /// Authorization role assigned to this connection.
        role: ClientRole,
        /// Maximum encoded payload size in one frame.
        max_frame_bytes: u64,
    },
    /// Authenticated liveness response.
    Pong,
    /// Confirms the selected Database.
    DatabaseSelected {
        /// Selected Database summary.
        database: DatabaseSummary,
    },
    /// A complete query result represented by the stable JSON wire shape.
    Result {
        /// Read or write statement result.
        statement: Value,
    },
    /// Starts a bounded read-result stream.
    StreamStart {
        /// Projected columns in order.
        columns: Vec<String>,
        /// Whether the query included an explicit ordering guarantee.
        ordered: bool,
    },
    /// One row in a read-result stream.
    StreamRow {
        /// Values in projected column order.
        row: Vec<Value>,
    },
    /// Ends a bounded read-result stream.
    StreamEnd {
        /// Number of row events emitted.
        rows: u64,
    },
    /// Confirms a query was queued in the active transaction.
    Queued {
        /// Number of statements currently queued.
        statements: u64,
    },
    /// Reports explicit transaction state.
    Transaction {
        /// `begun`, `committed`, or `rolled_back`.
        state: String,
        /// Results returned by a commit, empty otherwise.
        #[serde(default)]
        results: Vec<Value>,
    },
    /// Confirms a cooperative cancellation request was delivered.
    Cancelled {
        /// Target request identifier.
        target_request_id: u64,
    },
    /// One Database was created.
    DatabaseCreated {
        /// Created Database summary.
        database: DatabaseSummary,
    },
    /// Complete deterministic catalog listing.
    DatabaseList {
        /// Databases ordered by name.
        databases: Vec<DatabaseSummary>,
    },
    /// Detailed Database health and storage metadata.
    DatabaseInfo {
        /// Inspected Database details.
        database: DatabaseDetails,
    },
    /// One Database was renamed.
    DatabaseRenamed {
        /// Updated Database summary.
        database: DatabaseSummary,
    },
    /// One Database was removed from the active catalog.
    DatabaseDropped {
        /// Stable identity of the removed Database.
        database_id: String,
        /// Former catalog name.
        name: String,
    },
    /// Starts a physical snapshot download.
    SnapshotStart {
        /// Exact decoded snapshot size.
        total_bytes: u64,
    },
    /// One base64-encoded snapshot download chunk.
    SnapshotChunk {
        /// Zero-based contiguous chunk sequence.
        sequence: u64,
        /// Standard padded base64 payload.
        data: String,
    },
    /// Ends a physical snapshot download.
    SnapshotEnd {
        /// Number of chunks emitted.
        chunks: u64,
    },
    /// Reports snapshot upload progress or installation.
    SnapshotRestore {
        /// `ready`, `chunk_accepted`, `restored`, or `aborted`.
        state: String,
        /// Decoded bytes accepted so far.
        bytes: u64,
    },
    /// Returns one versioned portable logical package document.
    LogicalPackage {
        /// Serialized package.
        package: Value,
    },
    /// Confirms a logical package import.
    LogicalImported {
        /// Number of `.nostdb` modules imported.
        modules: u64,
    },
    /// Stable typed failure. The connection remains usable unless the protocol closes it.
    Error {
        /// Stable machine-readable category.
        code: ErrorCode,
        /// Human-readable diagnostic with no secret content.
        message: String,
        /// Whether retrying later without changing the request may succeed.
        retryable: bool,
    },
}

/// Authorization role assigned during authentication.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientRole {
    /// Query, transaction, liveness, and Database-selection permission.
    Query,
    /// Query permission plus Database lifecycle and import/export administration.
    Admin,
}

/// Stable protocol error categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Client requested a protocol version the server does not implement.
    UnsupportedProtocol,
    /// Credential was absent or invalid.
    AuthenticationFailed,
    /// Frame order or content violates the negotiated protocol.
    ProtocolViolation,
    /// The authenticated role does not permit this operation.
    PermissionDenied,
    /// A query operation has no selected Database.
    DatabaseNotSelected,
    /// No Database has the requested name.
    DatabaseNotFound,
    /// A create or rename target already exists.
    DatabaseAlreadyExists,
    /// A Database name violates the catalog naming contract.
    InvalidDatabaseName,
    /// A required ownership or lifecycle lock is unavailable.
    DatabaseBusy,
    /// Query syntax, semantics, types, evaluation, or constraints failed.
    QueryError,
    /// A configured resource or time limit stopped execution.
    ResourceLimit,
    /// Cooperative cancellation stopped execution.
    Cancelled,
    /// `BEGIN` was requested while a transaction is active.
    TransactionAlreadyActive,
    /// `COMMIT` or `ROLLBACK` was requested without an active transaction.
    NoTransaction,
    /// A frame or accumulated upload exceeds a configured bound.
    RequestTooLarge,
    /// A physical snapshot is corrupt or format-incompatible.
    SnapshotIncompatible,
    /// Startup or lifecycle recovery requires operator intervention.
    RecoveryRequired,
    /// An unexpected internal failure occurred.
    InternalError,
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let encoded = serde_json::to_value(self).map_err(|_| fmt::Error)?;
        formatter.write_str(encoded.as_str().ok_or(fmt::Error)?)
    }
}

/// Query limits sent by a client. Every value can only lower the server default.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WireQueryLimits {
    /// Maximum result rows.
    #[serde(default)]
    pub max_rows: Option<u64>,
    /// Maximum estimated materialized bytes.
    #[serde(default)]
    pub max_memory_bytes: Option<u64>,
    /// Maximum evaluator and row-processing work units.
    #[serde(default)]
    pub max_operations: Option<u64>,
    /// Maximum relationship candidates examined.
    #[serde(default)]
    pub max_traversals: Option<u64>,
}

/// Catalog-safe Database summary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DatabaseSummary {
    /// Immutable stable Database identity.
    pub id: String,
    /// Unique user-facing catalog name.
    pub name: String,
    /// Durable lifecycle state, currently `ready`.
    pub state: String,
}

/// Detailed Database health and storage metadata without filesystem paths.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DatabaseDetails {
    /// Catalog summary.
    #[serde(flatten)]
    pub summary: DatabaseSummary,
    /// Independent `.ndb` format version.
    pub ndb_format_version: u32,
    /// Backend schema revision.
    pub schema_revision: u32,
    /// Committed write generation.
    pub generation: u64,
    /// Fixed-width lowercase logical checksum.
    pub logical_checksum: String,
    /// Whether integrity checks passed.
    pub healthy: bool,
    /// Durable Schema count.
    pub schemas: u64,
    /// Durable Node count.
    pub nodes: u64,
    /// Durable Edge count.
    pub edges: u64,
}

/// Blocking client failure.
#[derive(Debug)]
pub enum ClientError {
    /// Address, socket, or frame I/O failure.
    Io(io::Error),
    /// Malformed, oversized, or out-of-order protocol data.
    Protocol(String),
    /// Stable error returned by the server.
    Server {
        /// Stable machine-readable category.
        code: ErrorCode,
        /// Server diagnostic.
        message: String,
        /// Whether retrying later may succeed.
        retryable: bool,
    },
}

impl fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "database connection failed: {error}"),
            Self::Protocol(message) => write!(formatter, "database protocol error: {message}"),
            Self::Server { code, message, .. } => write!(formatter, "{code}: {message}"),
        }
    }
}

impl Error for ClientError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Protocol(_) | Self::Server { .. } => None,
        }
    }
}

impl From<io::Error> for ClientError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// One authenticated blocking database connection.
pub struct Client {
    stream: TcpStream,
    next_request_id: u64,
    role: ClientRole,
}

impl Client {
    /// Connects, negotiates exact protocol version 1, and authenticates.
    pub fn connect(
        server: &str,
        credential: impl Into<String>,
        client_name: impl Into<String>,
    ) -> Result<Self, ClientError> {
        let address = parse_server_address(server)?;
        let mut addresses = address.to_socket_addrs().map_err(ClientError::Io)?;
        let address = addresses.next().ok_or_else(|| {
            ClientError::Protocol("server address resolved to no endpoint".into())
        })?;
        let stream = TcpStream::connect_timeout(&address, Duration::from_secs(10))?;
        stream.set_nodelay(true)?;
        let mut client = Self {
            stream,
            next_request_id: 1,
            role: ClientRole::Query,
        };
        let response = client.request(ClientRequest::Hello {
            protocol_version: DATABASE_PROTOCOL_VERSION,
            credential: credential.into(),
            client_name: client_name.into(),
        })?;
        let ServerResponse::Hello {
            protocol_version,
            role,
            max_frame_bytes,
            ..
        } = response
        else {
            return Err(ClientError::Protocol(
                "server did not answer hello with hello".into(),
            ));
        };
        if protocol_version != DATABASE_PROTOCOL_VERSION
            || max_frame_bytes as usize != MAX_FRAME_BYTES
        {
            return Err(ClientError::Protocol(
                "server negotiated an incompatible framing contract".into(),
            ));
        }
        client.role = role;
        Ok(client)
    }

    /// Returns the role authenticated for this connection.
    #[must_use]
    pub const fn role(&self) -> ClientRole {
        self.role
    }

    /// Sends one request and returns its connection-local identifier.
    pub fn send(&mut self, request: ClientRequest) -> Result<u64, ClientError> {
        let request_id = self.next_request_id;
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .ok_or_else(|| ClientError::Protocol("request identifier exhausted".into()))?;
        write_frame(
            &mut self.stream,
            &ClientFrame {
                request_id,
                request,
            },
        )?;
        Ok(request_id)
    }

    /// Reads one server event.
    pub fn read(&mut self) -> Result<ServerFrame, ClientError> {
        read_frame(&mut self.stream)
    }

    /// Sends one request and reads one non-streaming response.
    pub fn request(&mut self, request: ClientRequest) -> Result<ServerResponse, ClientError> {
        let request_id = self.send(request)?;
        let frame = self.read()?;
        if frame.request_id != request_id {
            return Err(ClientError::Protocol(format!(
                "expected response {request_id}, received {}",
                frame.request_id
            )));
        }
        match frame.response {
            ServerResponse::Error {
                code,
                message,
                retryable,
            } => Err(ClientError::Server {
                code,
                message,
                retryable,
            }),
            response => Ok(response),
        }
    }
}

/// Parses `nostdb://HOST:PORT` without accepting paths, credentials, or fragments.
pub fn parse_server_address(server: &str) -> Result<String, ClientError> {
    let address = server
        .strip_prefix("nostdb://")
        .ok_or_else(|| ClientError::Protocol("server address must start with nostdb://".into()))?;
    if address.is_empty()
        || address.contains(['/', '?', '#', '@'])
        || (!address.starts_with('[') && !address.contains(':'))
    {
        return Err(ClientError::Protocol(
            "server address must contain only HOST:PORT".into(),
        ));
    }
    Ok(address.to_owned())
}

/// Encodes one value as a length-prefixed JSON frame.
pub fn write_frame<T: Serialize>(writer: &mut impl Write, value: &T) -> Result<(), ClientError> {
    let payload = serde_json::to_vec(value)
        .map_err(|error| ClientError::Protocol(format!("cannot encode frame: {error}")))?;
    if payload.is_empty() || payload.len() > MAX_FRAME_BYTES {
        return Err(ClientError::Protocol(format!(
            "encoded frame size {} is outside 1..={MAX_FRAME_BYTES}",
            payload.len()
        )));
    }
    let length = u32::try_from(payload.len())
        .map_err(|_| ClientError::Protocol("frame length is not representable".into()))?;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()?;
    Ok(())
}

/// Decodes one length-prefixed JSON frame.
pub fn read_frame<T: for<'de> Deserialize<'de>>(reader: &mut impl Read) -> Result<T, ClientError> {
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > MAX_FRAME_BYTES {
        return Err(ClientError::Protocol(format!(
            "received frame size {length} is outside 1..={MAX_FRAME_BYTES}"
        )));
    }
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload)?;
    serde_json::from_slice(&payload)
        .map_err(|error| ClientError::Protocol(format!("cannot decode frame: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_round_trips_and_rejects_unbounded_lengths() {
        let frame = ClientFrame {
            request_id: 9,
            request: ClientRequest::Ping,
        };
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &frame).expect("frame encodes");
        assert_eq!(
            read_frame::<ClientFrame>(&mut bytes.as_slice()).expect("frame decodes"),
            frame
        );

        let mut oversized = ((MAX_FRAME_BYTES as u32) + 1).to_be_bytes().to_vec();
        oversized.extend_from_slice(b"{}");
        assert!(matches!(
            read_frame::<ClientFrame>(&mut oversized.as_slice()),
            Err(ClientError::Protocol(_))
        ));
    }

    #[test]
    fn server_address_requires_the_database_scheme_and_no_path() {
        assert_eq!(
            parse_server_address("nostdb://127.0.0.1:7878").expect("address parses"),
            "127.0.0.1:7878"
        );
        for invalid in [
            "127.0.0.1:7878",
            "nostdb://127.0.0.1:7878/path",
            "nostdb://user@127.0.0.1:7878",
        ] {
            assert!(parse_server_address(invalid).is_err(), "{invalid}");
        }
    }
}
