//! Versioned daemon configuration and protected credential files.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use nostdb_client::ClientRole;
use nostdb_engine::QueryLimits;
use serde::{Deserialize, Serialize};

use crate::ServerError;

/// Current version of `server.toml`.
pub const CONFIG_VERSION: u32 = 1;

/// Complete versioned `nostd` configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    /// Configuration format version, independent from every other version.
    pub config_version: u32,
    /// Daemon-owned directory containing catalog, Databases, locks, and recovery state.
    pub data_directory: PathBuf,
    /// Database protocol listener settings.
    pub network: NetworkConfig,
    /// Protected credential-file locations.
    pub authentication: AuthenticationConfig,
    /// Bounded runtime resources.
    pub limits: LimitConfig,
}

/// Database protocol listener settings.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfig {
    /// `IP:PORT` address. Initialization defaults to loopback.
    pub listen: String,
}

/// Paths to separate query and administrative credentials.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthenticationConfig {
    /// File containing the ordinary query credential and no other content.
    pub query_credential_file: PathBuf,
    /// File containing the administrative credential and no other content.
    pub admin_credential_file: PathBuf,
}

/// Resource limits applied before any client-provided lowering.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitConfig {
    /// Maximum concurrently retained network connections.
    pub max_connections: usize,
    /// Maximum concurrently retained connection-local sessions.
    pub max_sessions: usize,
    /// Maximum queued statements in one explicit transaction.
    pub max_transaction_statements: usize,
    /// Wall-clock query timeout.
    pub query_timeout_ms: u64,
    /// Maximum rows in one statement or transaction batch.
    pub max_rows: u64,
    /// Maximum estimated materialized bytes.
    pub max_memory_bytes: u64,
    /// Maximum evaluator and row-processing work units.
    pub max_operations: u64,
    /// Maximum relationship candidates examined.
    pub max_traversals: u64,
    /// Maximum decoded physical snapshot upload size.
    pub max_snapshot_bytes: u64,
}

impl DaemonConfig {
    /// Creates conservative local-only defaults for one data directory.
    #[must_use]
    pub fn new(data_directory: PathBuf, listen: String) -> Self {
        let credential_directory = data_directory.join("credentials");
        Self {
            config_version: CONFIG_VERSION,
            data_directory,
            network: NetworkConfig { listen },
            authentication: AuthenticationConfig {
                query_credential_file: credential_directory.join("client.token"),
                admin_credential_file: credential_directory.join("admin.token"),
            },
            limits: LimitConfig {
                max_connections: 256,
                max_sessions: 1024,
                max_transaction_statements: 1000,
                query_timeout_ms: 30_000,
                max_rows: 10_000,
                max_memory_bytes: 64 * 1024 * 1024,
                max_operations: 10_000_000,
                max_traversals: 1_000_000,
                max_snapshot_bytes: 1024 * 1024 * 1024,
            },
        }
    }

    /// Loads and validates an existing configuration file.
    pub fn load(path: &Path) -> Result<Self, ServerError> {
        let source = fs::read_to_string(path).map_err(|error| {
            ServerError::new(format!(
                "cannot read configuration {}: {error}",
                path.display()
            ))
        })?;
        let mut config: Self = toml::from_str(&source).map_err(|error| {
            ServerError::new(format!("invalid configuration {}: {error}", path.display()))
        })?;
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        config.data_directory = resolve_path(base, &config.data_directory)?;
        config.authentication.query_credential_file =
            resolve_path(base, &config.authentication.query_credential_file)?;
        config.authentication.admin_credential_file =
            resolve_path(base, &config.authentication.admin_credential_file)?;
        config.validate()?;
        Ok(config)
    }

    /// Writes a new configuration without replacing an existing file.
    pub fn write_new(&self, path: &Path) -> Result<(), ServerError> {
        self.validate()?;
        if let Some(parent) = path.parent().filter(|value| !value.as_os_str().is_empty()) {
            fs::create_dir_all(parent).map_err(|error| {
                ServerError::new(format!(
                    "cannot create configuration directory {}: {error}",
                    parent.display()
                ))
            })?;
        }
        let source = toml::to_string_pretty(self)
            .map_err(|error| ServerError::new(format!("cannot encode configuration: {error}")))?;
        write_new_file(path, source.as_bytes(), false)
    }

    /// Parses the configured listener.
    pub fn listen_address(&self) -> Result<SocketAddr, ServerError> {
        self.network.listen.parse().map_err(|error| {
            ServerError::new(format!(
                "invalid network.listen `{}`: {error}",
                self.network.listen
            ))
        })
    }

    /// Returns default Core query limits.
    #[must_use]
    pub const fn query_limits(&self) -> QueryLimits {
        QueryLimits {
            max_rows: self.limits.max_rows,
            max_memory_bytes: self.limits.max_memory_bytes,
            max_operations: self.limits.max_operations,
            max_traversals: self.limits.max_traversals,
        }
    }

    /// Returns the wall-clock query timeout.
    #[must_use]
    pub const fn query_timeout(&self) -> Duration {
        Duration::from_millis(self.limits.query_timeout_ms)
    }

    fn validate(&self) -> Result<(), ServerError> {
        if self.config_version != CONFIG_VERSION {
            return Err(ServerError::new(format!(
                "unsupported config_version {}; this binary supports exactly {CONFIG_VERSION}",
                self.config_version
            )));
        }
        let listen = self.listen_address()?;
        if self.network.listen.trim() != listen.to_string() {
            return Err(ServerError::new(
                "network.listen must be a canonical numeric IP:PORT address",
            ));
        }
        if self.data_directory.as_os_str().is_empty() {
            return Err(ServerError::new("data_directory must not be empty"));
        }
        if self.authentication.query_credential_file == self.authentication.admin_credential_file {
            return Err(ServerError::new(
                "query and admin credential files must be different",
            ));
        }
        let limits = &self.limits;
        if limits.max_connections == 0
            || limits.max_sessions == 0
            || limits.max_transaction_statements == 0
            || limits.query_timeout_ms == 0
            || limits.max_rows == 0
            || limits.max_memory_bytes == 0
            || limits.max_operations == 0
            || limits.max_traversals == 0
            || limits.max_snapshot_bytes == 0
        {
            return Err(ServerError::new("all daemon limits must be positive"));
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct Credentials {
    query: String,
    admin: String,
}

impl Credentials {
    pub(crate) fn load(config: &DaemonConfig) -> Result<Self, ServerError> {
        let query = read_credential(&config.authentication.query_credential_file)?;
        let admin = read_credential(&config.authentication.admin_credential_file)?;
        if query == admin {
            return Err(ServerError::new(
                "query and admin credentials must not have the same value",
            ));
        }
        Ok(Self { query, admin })
    }

    pub(crate) fn authenticate(&self, supplied: &str) -> Option<ClientRole> {
        if constant_time_equal(supplied.as_bytes(), self.admin.as_bytes()) {
            Some(ClientRole::Admin)
        } else if constant_time_equal(supplied.as_bytes(), self.query.as_bytes()) {
            Some(ClientRole::Query)
        } else {
            None
        }
    }
}

pub(crate) fn write_credential(path: &Path, credential: &str) -> Result<(), ServerError> {
    write_new_file(path, format!("{credential}\n").as_bytes(), true)
}

fn read_credential(path: &Path) -> Result<String, ServerError> {
    #[cfg(unix)]
    validate_credential_permissions(path)?;
    let credential = fs::read_to_string(path).map_err(|error| {
        ServerError::new(format!(
            "cannot read credential file {}: {error}",
            path.display()
        ))
    })?;
    let credential = credential.trim_end_matches(['\r', '\n']);
    if credential.len() < 32 || credential.chars().any(char::is_whitespace) {
        return Err(ServerError::new(format!(
            "credential file {} must contain one non-whitespace token of at least 32 characters",
            path.display()
        )));
    }
    Ok(credential.to_owned())
}

#[cfg(unix)]
fn validate_credential_permissions(path: &Path) -> Result<(), ServerError> {
    use std::os::unix::fs::PermissionsExt;

    let mode = fs::metadata(path)
        .map_err(|error| {
            ServerError::new(format!(
                "cannot inspect credential file {}: {error}",
                path.display()
            ))
        })?
        .permissions()
        .mode()
        & 0o777;
    if mode & 0o077 != 0 {
        return Err(ServerError::new(format!(
            "credential file {} has mode {mode:03o}; remove all group and other permissions (0600 or stricter is required)",
            path.display()
        )));
    }
    Ok(())
}

fn write_new_file(path: &Path, bytes: &[u8], protected: bool) -> Result<(), ServerError> {
    if let Some(parent) = path.parent().filter(|value| !value.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(|error| {
            ServerError::new(format!(
                "cannot create directory {}: {error}",
                parent.display()
            ))
        })?;
    }
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    if protected {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = protected;
    let mut file = options
        .open(path)
        .map_err(|error| ServerError::new(format!("cannot create {}: {error}", path.display())))?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        drop(file);
        return match fs::remove_file(path) {
            Ok(()) => Err(ServerError::new(format!(
                "cannot persist {}: {error}",
                path.display()
            ))),
            Err(cleanup_error) if cleanup_error.kind() == std::io::ErrorKind::NotFound => Err(
                ServerError::new(format!("cannot persist {}: {error}", path.display())),
            ),
            Err(cleanup_error) => Err(ServerError::new(format!(
                "cannot persist {}: {error}; cannot remove partial file: {cleanup_error}",
                path.display()
            ))),
        };
    }
    Ok(())
}

fn resolve_path(base: &Path, value: &Path) -> Result<PathBuf, ServerError> {
    let joined = if value.is_absolute() {
        value.to_path_buf()
    } else {
        base.join(value)
    };
    if joined.as_os_str().is_empty() {
        return Err(ServerError::new("configuration path must not be empty"));
    }
    Ok(joined)
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let maximum = left.len().max(right.len());
    for index in 0..maximum {
        difference |= usize::from(
            left.get(index).copied().unwrap_or_default()
                ^ right.get(index).copied().unwrap_or_default(),
        );
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_round_trips_and_rejects_unknown_versions() {
        let root = std::env::temp_dir().join(format!(
            "nostdb-config-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir(&root).expect("test directory creates");
        let path = root.join("server.toml");
        let config = DaemonConfig::new(root.join("data"), "127.0.0.1:0".into());
        config.write_new(&path).expect("configuration writes");
        assert_eq!(
            DaemonConfig::load(&path).expect("configuration loads"),
            config
        );

        let invalid = fs::read_to_string(&path)
            .expect("configuration reads")
            .replacen("config_version = 1", "config_version = 999", 1);
        fs::write(&path, invalid).expect("configuration changes");
        assert!(DaemonConfig::load(&path).is_err());
        fs::remove_dir_all(root).expect("test directory removes");
    }

    #[test]
    fn credential_comparison_covers_equal_and_different_lengths() {
        assert!(constant_time_equal(b"same", b"same"));
        assert!(!constant_time_equal(b"same", b"other"));
        assert!(!constant_time_equal(b"short", b"longer"));
    }
}
