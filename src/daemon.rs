//! Managed data-directory runtime used by the database protocol and operators.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use fs2::FileExt;
use nostdb_client::{ClientRole, DatabaseDetails, DatabaseSummary, ErrorCode, WireQueryLimits};
use nostdb_engine::{
    CancellationToken, DatabaseError, EmbeddedDatabase, Parameters, QueryErrorCode, QueryLimits,
    StatementResult, StorageErrorKind, prepare_write,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::catalog::{CatalogDatabase, CatalogStore, OperationKind, valid_database_name};
use crate::config::{Credentials, DaemonConfig, write_credential};
use crate::{ServerError, wire};

/// Non-secret paths created by `nostd init`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitializationReport {
    /// Newly written configuration file.
    pub config_path: PathBuf,
    /// Initialized daemon-owned data directory.
    pub data_directory: PathBuf,
    /// Ordinary client credential file. Its value is never returned here.
    pub query_credential_file: PathBuf,
    /// Administrative credential file. Its value is never returned here.
    pub admin_credential_file: PathBuf,
}

/// Stable failure sent through the database protocol.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProtocolFailure {
    pub(crate) code: ErrorCode,
    pub(crate) message: String,
    pub(crate) retryable: bool,
}

impl ProtocolFailure {
    pub(crate) fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: false,
        }
    }

    pub(crate) fn retryable(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: true,
        }
    }
}

pub(crate) struct ManagedDatabase {
    id: String,
    path: PathBuf,
    database: Mutex<Option<EmbeddedDatabase>>,
}

impl ManagedDatabase {
    fn new(id: String, path: PathBuf, database: EmbeddedDatabase) -> Self {
        Self {
            id,
            path,
            database: Mutex::new(Some(database)),
        }
    }
}

/// One running, exclusively owned data directory and its managed Databases.
pub struct DatabaseDaemon {
    config: Arc<DaemonConfig>,
    credentials: Credentials,
    catalog: Mutex<CatalogStore>,
    databases: RwLock<BTreeMap<String, Arc<ManagedDatabase>>>,
    _data_directory_lock: File,
}

impl DatabaseDaemon {
    /// Initializes a fresh data directory, protected credentials, and config.
    pub fn initialize(
        config_path: &Path,
        data_directory: &Path,
        listen: &str,
    ) -> Result<InitializationReport, ServerError> {
        let data_directory = absolute(data_directory)?;
        let config_path = absolute(config_path)?;
        let config = DaemonConfig::new(data_directory.clone(), listen.to_owned());
        config.listen_address()?;
        ensure_new_path(&config_path, "configuration")?;
        let data_directory_existed = data_directory.exists();
        CatalogStore::initialize(&data_directory)?;

        let query_credential = generate_credential();
        let admin_credential = generate_credential();
        let mut query_credential_created = false;
        let mut admin_credential_created = false;
        let result = (|| {
            write_credential(
                &config.authentication.query_credential_file,
                &query_credential,
            )?;
            query_credential_created = true;
            write_credential(
                &config.authentication.admin_credential_file,
                &admin_credential,
            )?;
            admin_credential_created = true;
            config.write_new(&config_path)
        })();
        if let Err(error) = result {
            return match rollback_initialization(
                &config,
                data_directory_existed,
                query_credential_created,
                admin_credential_created,
            ) {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(ServerError::new(format!(
                    "{error}; initialization rollback was incomplete: {rollback_error}"
                ))),
            };
        }
        Ok(InitializationReport {
            config_path,
            data_directory,
            query_credential_file: config.authentication.query_credential_file,
            admin_credential_file: config.authentication.admin_credential_file,
        })
    }

    /// Acquires exclusive ownership, recovers completed operations, and opens all Databases.
    pub fn open(config: DaemonConfig) -> Result<Arc<Self>, ServerError> {
        let data_lock = acquire_data_directory_lock(&config.data_directory)?;
        recover_snapshot_operations(&config.data_directory)?;
        let credentials = Credentials::load(&config)?;
        let catalog = CatalogStore::load(&config.data_directory)?;
        let mut databases = BTreeMap::new();
        for entry in &catalog.catalog().databases {
            let path = catalog.database_path(&entry.id);
            let database = EmbeddedDatabase::open(&path).map_err(|error| {
                ServerError::new(format!(
                    "cannot open managed Database `{}`: {error}",
                    entry.name
                ))
            })?;
            let info = database.info().map_err(|error| {
                ServerError::new(format!(
                    "cannot inspect managed Database `{}`: {error}",
                    entry.name
                ))
            })?;
            if info.source_managed {
                return Err(ServerError::new(format!(
                    "managed Database `{}` still has Source Mode authority; import it explicitly",
                    entry.name
                )));
            }
            databases.insert(
                entry.id.clone(),
                Arc::new(ManagedDatabase::new(entry.id.clone(), path, database)),
            );
        }
        Ok(Arc::new(Self {
            config: Arc::new(config),
            credentials,
            catalog: Mutex::new(catalog),
            databases: RwLock::new(databases),
            _data_directory_lock: data_lock,
        }))
    }

    /// Returns the immutable runtime configuration.
    #[must_use]
    pub fn config(&self) -> &DaemonConfig {
        &self.config
    }

    pub(crate) fn authenticate(&self, credential: &str) -> Option<ClientRole> {
        self.credentials.authenticate(credential)
    }

    pub(crate) fn list_databases(&self) -> Result<Vec<DatabaseSummary>, ProtocolFailure> {
        let catalog = self.catalog_lock()?;
        Ok(catalog
            .catalog()
            .databases
            .iter()
            .map(CatalogDatabase::summary)
            .collect())
    }

    pub(crate) fn select_database(&self, name: &str) -> Result<DatabaseSummary, ProtocolFailure> {
        let catalog = self.catalog_lock()?;
        catalog
            .catalog()
            .databases
            .iter()
            .find(|entry| entry.name == name)
            .map(CatalogDatabase::summary)
            .ok_or_else(|| {
                ProtocolFailure::new(
                    ErrorCode::DatabaseNotFound,
                    format!("Database `{name}` does not exist"),
                )
            })
    }

    pub(crate) fn create_database(&self, name: &str) -> Result<DatabaseSummary, ProtocolFailure> {
        if !valid_database_name(name) {
            return Err(ProtocolFailure::new(
                ErrorCode::InvalidDatabaseName,
                "Database names must match [a-z][a-z0-9_-]{0,62}",
            ));
        }
        let mut catalog = self.catalog_lock()?;
        if catalog
            .catalog()
            .databases
            .iter()
            .any(|entry| entry.name == name)
        {
            return Err(ProtocolFailure::new(
                ErrorCode::DatabaseAlreadyExists,
                format!("Database `{name}` already exists"),
            ));
        }
        let id = Uuid::new_v4().to_string();
        let directory = catalog.database_directory(&id);
        fs::create_dir(&directory).map_err(internal)?;
        let path = catalog.database_path(&id);
        let database = match EmbeddedDatabase::create(&path) {
            Ok(database) => database,
            Err(error) => {
                let _ = fs::remove_dir(&directory);
                return Err(database_failure(error, None));
            }
        };
        let entry = CatalogDatabase {
            id: id.clone(),
            name: name.to_owned(),
            state: "ready".to_owned(),
        };
        let mut next = catalog.catalog().clone();
        next.databases.push(entry.clone());
        next.databases
            .sort_by(|left, right| left.name.cmp(&right.name));
        if let Err(error) = catalog.transition(next, OperationKind::Create, &id) {
            drop(database);
            let target = self
                .config
                .data_directory
                .join("recovery")
                .join(format!("failed-create-{id}"));
            let _ = fs::rename(&directory, target);
            return Err(internal(error));
        }
        self.databases_write()?.insert(
            id.clone(),
            Arc::new(ManagedDatabase::new(id, path, database)),
        );
        Ok(preserve_committed_lifecycle_result(
            entry.summary(),
            "create",
            catalog.finish_transition(),
        ))
    }

    pub(crate) fn rename_database(
        &self,
        name: &str,
        new_name: &str,
    ) -> Result<DatabaseSummary, ProtocolFailure> {
        if !valid_database_name(new_name) {
            return Err(ProtocolFailure::new(
                ErrorCode::InvalidDatabaseName,
                "Database names must match [a-z][a-z0-9_-]{0,62}",
            ));
        }
        let mut catalog = self.catalog_lock()?;
        if catalog
            .catalog()
            .databases
            .iter()
            .any(|entry| entry.name == new_name)
        {
            return Err(ProtocolFailure::new(
                ErrorCode::DatabaseAlreadyExists,
                format!("Database `{new_name}` already exists"),
            ));
        }
        let existing = catalog
            .catalog()
            .databases
            .iter()
            .find(|entry| entry.name == name)
            .cloned()
            .ok_or_else(|| not_found(name))?;
        let mut next = catalog.catalog().clone();
        let entry = next
            .databases
            .iter_mut()
            .find(|entry| entry.id == existing.id)
            .expect("copied catalog contains the selected Database");
        entry.name = new_name.to_owned();
        let updated = entry.clone();
        next.databases
            .sort_by(|left, right| left.name.cmp(&right.name));
        catalog
            .transition(next, OperationKind::Rename, &existing.id)
            .map_err(internal)?;
        Ok(preserve_committed_lifecycle_result(
            updated.summary(),
            "rename",
            catalog.finish_transition(),
        ))
    }

    pub(crate) fn drop_database(
        &self,
        name: &str,
        confirm_name: &str,
    ) -> Result<DatabaseSummary, ProtocolFailure> {
        self.drop_database_with_checkpoint(name, confirm_name, |database| {
            database
                .checkpoint()
                .map_err(|error| database_failure(error, None))
        })
    }

    fn drop_database_with_checkpoint(
        &self,
        name: &str,
        confirm_name: &str,
        checkpoint: impl FnOnce(&mut EmbeddedDatabase) -> Result<(), ProtocolFailure>,
    ) -> Result<DatabaseSummary, ProtocolFailure> {
        if name != confirm_name {
            return Err(ProtocolFailure::new(
                ErrorCode::ProtocolViolation,
                "confirm_name must exactly equal the Database name",
            ));
        }
        let mut catalog = self.catalog_lock()?;
        let entry = catalog
            .catalog()
            .databases
            .iter()
            .find(|entry| entry.name == name)
            .cloned()
            .ok_or_else(|| not_found(name))?;
        let directory = catalog.database_directory(&entry.id);
        let trash = catalog.trash_directory(&entry.id);
        if trash.exists() {
            return Err(internal("Database trash target already exists"));
        }
        let mut databases = self.databases_write()?;
        let handle = databases
            .get(&entry.id)
            .cloned()
            .ok_or_else(|| internal("managed Database handle is missing"))?;
        let mut guard = handle
            .database
            .lock()
            .map_err(|_| internal("managed Database lock is poisoned"))?;
        let mut database = guard.take().ok_or_else(|| {
            ProtocolFailure::retryable(ErrorCode::DatabaseBusy, "Database is unavailable")
        })?;
        if let Err(error) = checkpoint(&mut database) {
            *guard = Some(database);
            return Err(error);
        }
        drop(database);
        drop(guard);
        databases.remove(&entry.id);
        drop(databases);

        let mut next = catalog.catalog().clone();
        next.databases.retain(|candidate| candidate.id != entry.id);
        if let Err(error) = catalog.transition(next, OperationKind::Drop, &entry.id) {
            let reopened = EmbeddedDatabase::open(&handle.path).map_err(|open_error| {
                internal(format!(
                    "catalog drop failed ({error}) and Database could not reopen: {open_error}"
                ))
            })?;
            *handle
                .database
                .lock()
                .map_err(|_| internal("managed Database lock is poisoned"))? = Some(reopened);
            self.databases_write()?.insert(entry.id.clone(), handle);
            return Err(internal(error));
        }
        fs::rename(directory, trash).map_err(internal)?;
        Ok(preserve_committed_lifecycle_result(
            entry.summary(),
            "drop",
            catalog.finish_transition(),
        ))
    }

    pub(crate) fn inspect_database(&self, name: &str) -> Result<DatabaseDetails, ProtocolFailure> {
        let entry = self.entry(name)?;
        let handle = self.handle(&entry.id)?;
        let guard = handle
            .database
            .lock()
            .map_err(|_| internal("managed Database lock is poisoned"))?;
        let database = guard.as_ref().ok_or_else(|| {
            ProtocolFailure::retryable(ErrorCode::DatabaseBusy, "Database is unavailable")
        })?;
        let info = database
            .info()
            .map_err(|error| database_failure(error, None))?;
        let counts = database
            .counts()
            .map_err(|error| database_failure(error, None))?;
        let healthy = database
            .check()
            .map_err(|error| database_failure(error, None))?
            .is_valid();
        Ok(DatabaseDetails {
            summary: entry.summary(),
            ndb_format_version: info.ndb_format_version,
            schema_revision: info.schema_revision,
            generation: info.generation,
            logical_checksum: format!("{:016x}", info.logical_checksum),
            healthy,
            schemas: counts.schemas,
            nodes: counts.nodes,
            edges: counts.edges,
        })
    }

    pub(crate) fn execute_selected(
        &self,
        database_id: &str,
        query: &str,
        parameters: BTreeMap<String, Value>,
        read_only: bool,
        requested_limits: Option<WireQueryLimits>,
        cancellation: CancellationToken,
    ) -> Result<StatementResult, ProtocolFailure> {
        if read_only && prepare_write(query).is_ok() {
            return Err(ProtocolFailure::new(
                ErrorCode::QueryError,
                "read_only request rejected a mutating query",
            ));
        }
        let parameters = wire::parameters(parameters)
            .map_err(|message| ProtocolFailure::new(ErrorCode::QueryError, message))?;
        let handle = self.handle_for_selected_id(database_id)?;
        let mut guard = handle
            .database
            .lock()
            .map_err(|_| internal("managed Database lock is poisoned"))?;
        let database = guard.as_mut().ok_or_else(|| {
            ProtocolFailure::retryable(ErrorCode::DatabaseBusy, "Database is unavailable")
        })?;
        let limits = lower_limits(self.config.query_limits(), requested_limits);
        database
            .execute_limited(query, &parameters, limits, cancellation.clone())
            .map_err(|error| database_failure(error, Some(&cancellation)))
    }

    pub(crate) fn execute_selected_transaction(
        &self,
        database_id: &str,
        statements: Vec<(String, BTreeMap<String, Value>)>,
        cancellation: CancellationToken,
    ) -> Result<Vec<StatementResult>, ProtocolFailure> {
        let statements = statements
            .into_iter()
            .map(|(query, parameters)| {
                wire::parameters(parameters)
                    .map(|parameters| (query, parameters))
                    .map_err(|message| ProtocolFailure::new(ErrorCode::QueryError, message))
            })
            .collect::<Result<Vec<(String, Parameters)>, _>>()?;
        let handle = self.handle_for_selected_id(database_id)?;
        let mut guard = handle
            .database
            .lock()
            .map_err(|_| internal("managed Database lock is poisoned"))?;
        let database = guard.as_mut().ok_or_else(|| {
            ProtocolFailure::retryable(ErrorCode::DatabaseBusy, "Database is unavailable")
        })?;
        database
            .execute_transaction_limited(
                &statements,
                self.config.query_limits(),
                cancellation.clone(),
            )
            .map_err(|error| database_failure(error, Some(&cancellation)))
    }

    pub(crate) fn export_snapshot(&self, name: &str) -> Result<Vec<u8>, ProtocolFailure> {
        let handle = self.handle_for_name(name)?;
        let mut guard = handle
            .database
            .lock()
            .map_err(|_| internal("managed Database lock is poisoned"))?;
        let database = guard.as_mut().ok_or_else(|| {
            ProtocolFailure::retryable(ErrorCode::DatabaseBusy, "Database is unavailable")
        })?;
        database
            .checkpoint()
            .map_err(|error| database_failure(error, None))?;
        fs::read(&handle.path).map_err(internal)
    }

    pub(crate) fn restore_snapshot(&self, name: &str, bytes: &[u8]) -> Result<(), ProtocolFailure> {
        let handle = self.handle_for_name(name)?;
        restore_snapshot(&handle, bytes)
    }

    pub(crate) fn export_logical(&self, name: &str) -> Result<Value, ProtocolFailure> {
        let handle = self.handle_for_name(name)?;
        let guard = handle
            .database
            .lock()
            .map_err(|_| internal("managed Database lock is poisoned"))?;
        let database = guard.as_ref().ok_or_else(|| {
            ProtocolFailure::retryable(ErrorCode::DatabaseBusy, "Database is unavailable")
        })?;
        let package = database
            .export_logical()
            .map_err(|error| ProtocolFailure::new(ErrorCode::QueryError, error.to_string()))?;
        serde_json::to_value(LogicalPackageDocument::from(package)).map_err(internal)
    }

    pub(crate) fn import_logical(
        &self,
        name: &str,
        package: Value,
    ) -> Result<u64, ProtocolFailure> {
        let package: LogicalPackageDocument = serde_json::from_value(package).map_err(|error| {
            ProtocolFailure::new(
                ErrorCode::QueryError,
                format!("invalid logical package: {error}"),
            )
        })?;
        let modules = u64::try_from(package.modules.len())
            .map_err(|_| ProtocolFailure::new(ErrorCode::RequestTooLarge, "too many modules"))?;
        let handle = self.handle_for_name(name)?;
        import_logical(&handle, package)?;
        Ok(modules)
    }

    fn entry(&self, name: &str) -> Result<CatalogDatabase, ProtocolFailure> {
        self.catalog_lock()?
            .catalog()
            .databases
            .iter()
            .find(|entry| entry.name == name)
            .cloned()
            .ok_or_else(|| not_found(name))
    }

    fn handle_for_name(&self, name: &str) -> Result<Arc<ManagedDatabase>, ProtocolFailure> {
        let entry = self.entry(name)?;
        self.handle(&entry.id)
    }

    fn handle_for_selected_id(
        &self,
        database_id: &str,
    ) -> Result<Arc<ManagedDatabase>, ProtocolFailure> {
        let selected_exists = self
            .catalog_lock()?
            .catalog()
            .databases
            .iter()
            .any(|entry| entry.id == database_id);
        if !selected_exists {
            return Err(ProtocolFailure::new(
                ErrorCode::DatabaseNotFound,
                "the selected Database no longer exists; select a Database again",
            ));
        }
        self.handle(database_id)
    }

    fn handle(&self, id: &str) -> Result<Arc<ManagedDatabase>, ProtocolFailure> {
        self.databases
            .read()
            .map_err(|_| internal("managed Database map is poisoned"))?
            .get(id)
            .cloned()
            .ok_or_else(|| {
                ProtocolFailure::retryable(ErrorCode::DatabaseBusy, "Database is unavailable")
            })
    }

    fn catalog_lock(&self) -> Result<std::sync::MutexGuard<'_, CatalogStore>, ProtocolFailure> {
        self.catalog
            .lock()
            .map_err(|_| internal("catalog lock is poisoned"))
    }

    fn databases_write(
        &self,
    ) -> Result<
        std::sync::RwLockWriteGuard<'_, BTreeMap<String, Arc<ManagedDatabase>>>,
        ProtocolFailure,
    > {
        self.databases
            .write()
            .map_err(|_| internal("managed Database map is poisoned"))
    }
}

fn acquire_data_directory_lock(root: &Path) -> Result<File, ServerError> {
    let path = root.join("locks/daemon.lock");
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|error| {
            ServerError::new(format!(
                "cannot open data-directory lock {}: {error}",
                path.display()
            ))
        })?;
    FileExt::try_lock_exclusive(&file).map_err(|error| {
        ServerError::new(format!(
            "data directory {} is already owned by another daemon: {error}",
            root.display()
        ))
    })?;
    file.set_len(0)
        .map_err(|error| ServerError::new(error.to_string()))?;
    writeln!(file, "pid={}", std::process::id())
        .and_then(|()| file.sync_all())
        .map_err(|error| {
            ServerError::new(format!("cannot persist daemon lock metadata: {error}"))
        })?;
    Ok(file)
}

fn generate_credential() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn lower_limits(defaults: QueryLimits, requested: Option<WireQueryLimits>) -> QueryLimits {
    let Some(requested) = requested else {
        return defaults;
    };
    QueryLimits {
        max_rows: requested
            .max_rows
            .map_or(defaults.max_rows, |value| defaults.max_rows.min(value)),
        max_memory_bytes: requested
            .max_memory_bytes
            .map_or(defaults.max_memory_bytes, |value| {
                defaults.max_memory_bytes.min(value)
            }),
        max_operations: requested
            .max_operations
            .map_or(defaults.max_operations, |value| {
                defaults.max_operations.min(value)
            }),
        max_traversals: requested
            .max_traversals
            .map_or(defaults.max_traversals, |value| {
                defaults.max_traversals.min(value)
            }),
    }
}

fn database_failure(
    error: DatabaseError,
    cancellation: Option<&CancellationToken>,
) -> ProtocolFailure {
    match error {
        DatabaseError::Query(error) if error.code() == QueryErrorCode::ResourceLimit => {
            if cancellation.is_some_and(CancellationToken::is_cancelled) {
                ProtocolFailure::new(ErrorCode::Cancelled, error.to_string())
            } else {
                ProtocolFailure::new(ErrorCode::ResourceLimit, error.to_string())
            }
        }
        DatabaseError::Query(error) => {
            ProtocolFailure::new(ErrorCode::QueryError, error.to_string())
        }
        DatabaseError::Storage(error) if error.kind() == StorageErrorKind::Busy => {
            ProtocolFailure::retryable(ErrorCode::DatabaseBusy, error.to_string())
        }
        DatabaseError::Storage(error) => {
            ProtocolFailure::new(ErrorCode::InternalError, error.to_string())
        }
    }
}

fn not_found(name: &str) -> ProtocolFailure {
    ProtocolFailure::new(
        ErrorCode::DatabaseNotFound,
        format!("Database `{name}` does not exist"),
    )
}

fn internal(error: impl std::fmt::Display) -> ProtocolFailure {
    ProtocolFailure::new(ErrorCode::InternalError, error.to_string())
}

fn absolute(path: &Path) -> Result<PathBuf, ServerError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|current| current.join(path))
            .map_err(|error| ServerError::new(format!("cannot resolve current directory: {error}")))
    }
}

fn ensure_new_path(path: &Path, description: &str) -> Result<(), ServerError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(ServerError::new(format!(
            "{description} path {} already exists",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ServerError::new(format!(
            "cannot inspect {description} path {}: {error}",
            path.display()
        ))),
    }
}

fn rollback_initialization(
    config: &DaemonConfig,
    data_directory_existed: bool,
    query_credential_created: bool,
    admin_credential_created: bool,
) -> Result<(), ServerError> {
    let mut failures = Vec::new();
    for (created, path) in [
        (
            admin_credential_created,
            &config.authentication.admin_credential_file,
        ),
        (
            query_credential_created,
            &config.authentication.query_credential_file,
        ),
    ] {
        if created {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    failures.push(format!("cannot remove {}: {error}", path.display()));
                }
            }
        }
    }
    let credential_directory = config.data_directory.join("credentials");
    match fs::remove_dir(&credential_directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            failures.push(format!(
                "cannot remove {}: {error}",
                credential_directory.display()
            ));
        }
    }
    if let Err(error) =
        CatalogStore::rollback_initialization(&config.data_directory, data_directory_existed)
    {
        failures.push(error.to_string());
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(ServerError::new(failures.join("; ")))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogicalPackageDocument {
    package_version: u32,
    language_version: u32,
    config: String,
    modules: Vec<LogicalModuleDocument>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogicalModuleDocument {
    path: String,
    stable_module_id: String,
    source: String,
}

impl From<nostdb_engine::LogicalPackage> for LogicalPackageDocument {
    fn from(package: nostdb_engine::LogicalPackage) -> Self {
        Self {
            package_version: package.package_version,
            language_version: 1,
            config: package.config,
            modules: package
                .modules
                .into_iter()
                .map(|module| LogicalModuleDocument {
                    path: module.path,
                    stable_module_id: module.module_id,
                    source: module.source,
                })
                .collect(),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RestoreJournal {
    operation_version: u32,
    database_id: String,
    stage: String,
}

fn restore_snapshot(handle: &ManagedDatabase, bytes: &[u8]) -> Result<(), ProtocolFailure> {
    restore_snapshot_with_operations(
        handle,
        bytes,
        |database| {
            database
                .checkpoint()
                .map_err(|error| database_failure(error, None))
        },
        |from, to| fs::rename(from, to),
        |path| fs::remove_file(path),
    )
}

fn restore_snapshot_with_operations(
    handle: &ManagedDatabase,
    bytes: &[u8],
    checkpoint: impl FnOnce(&mut EmbeddedDatabase) -> Result<(), ProtocolFailure>,
    mut rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
    mut remove: impl FnMut(&Path) -> std::io::Result<()>,
) -> Result<(), ProtocolFailure> {
    if bytes.is_empty() {
        return Err(ProtocolFailure::new(
            ErrorCode::SnapshotIncompatible,
            "snapshot is empty",
        ));
    }
    let directory = handle
        .path
        .parent()
        .ok_or_else(|| internal("managed Database path has no parent"))?;
    let candidate = directory.join("database.ndb.restore-candidate");
    let backup = directory.join("database.ndb.restore-backup");
    let journal = directory.join("restore-operation");
    cleanup_unjournaled_restore_candidate(directory).map_err(internal)?;
    for path in [&candidate, &backup, &journal] {
        if path.exists() {
            return Err(ProtocolFailure::retryable(
                ErrorCode::RecoveryRequired,
                "a previous snapshot restore requires daemon restart recovery",
            ));
        }
    }
    let result = (|| {
        write_new_synced(&candidate, bytes)?;
        let mut proposed = EmbeddedDatabase::open(&candidate).map_err(|error| {
            ProtocolFailure::new(ErrorCode::SnapshotIncompatible, error.to_string())
        })?;
        if !proposed
            .check()
            .map_err(|error| {
                ProtocolFailure::new(ErrorCode::SnapshotIncompatible, error.to_string())
            })?
            .is_valid()
        {
            return Err(ProtocolFailure::new(
                ErrorCode::SnapshotIncompatible,
                "snapshot integrity check failed",
            ));
        }
        proposed.adopt_server_authority().map_err(|error| {
            ProtocolFailure::new(ErrorCode::SnapshotIncompatible, error.to_string())
        })?;
        proposed.checkpoint().map_err(|error| {
            ProtocolFailure::new(ErrorCode::SnapshotIncompatible, error.to_string())
        })?;
        drop(proposed);
        write_restore_journal(&journal, &handle.id, "prepared")?;
        sync_restore_directory(directory).map_err(internal)?;

        let mut guard = handle
            .database
            .lock()
            .map_err(|_| internal("managed Database lock is poisoned"))?;
        let mut current = guard.take().ok_or_else(|| {
            ProtocolFailure::retryable(ErrorCode::DatabaseBusy, "Database is unavailable")
        })?;
        if let Err(error) = checkpoint(&mut current) {
            *guard = Some(current);
            return match cleanup_uncommitted_restore(directory, &candidate, &journal, &mut remove) {
                Ok(()) => Err(error),
                Err(cleanup_error) => Err(ProtocolFailure::retryable(
                    ErrorCode::RecoveryRequired,
                    format!(
                        "{}; restore cleanup requires restart: {cleanup_error}",
                        error.message
                    ),
                )),
            };
        }
        drop(current);
        if let Err(error) = rename(&handle.path, &backup) {
            return match EmbeddedDatabase::open(&handle.path) {
                Ok(database) => {
                    *guard = Some(database);
                    match cleanup_uncommitted_restore(directory, &candidate, &journal, &mut remove)
                    {
                        Ok(()) => Err(internal(format!(
                            "cannot move the live Database for snapshot installation: {error}"
                        ))),
                        Err(cleanup_error) => Err(ProtocolFailure::retryable(
                            ErrorCode::RecoveryRequired,
                            format!(
                                "cannot move the live Database for snapshot installation: {error}; restore cleanup requires restart: {cleanup_error}"
                            ),
                        )),
                    }
                }
                Err(open_error) => Err(ProtocolFailure::retryable(
                    ErrorCode::RecoveryRequired,
                    format!(
                        "cannot move the live Database for snapshot installation: {error}; the live Database could not reopen: {open_error}"
                    ),
                )),
            };
        }
        sync_restore_directory(directory).map_err(internal)?;
        if let Err(install_error) = rename(&candidate, &handle.path) {
            if let Err(rollback_error) = rename(&backup, &handle.path) {
                return Err(ProtocolFailure::retryable(
                    ErrorCode::RecoveryRequired,
                    format!(
                        "cannot install snapshot: {install_error}; cannot restore the live Database: {rollback_error}; restart recovery is required"
                    ),
                ));
            }
            sync_restore_directory(directory).map_err(internal)?;
            return match EmbeddedDatabase::open(&handle.path) {
                Ok(database) => {
                    *guard = Some(database);
                    match cleanup_uncommitted_restore(directory, &candidate, &journal, &mut remove)
                    {
                        Ok(()) => Err(internal(format!(
                            "cannot install snapshot: {install_error}"
                        ))),
                        Err(cleanup_error) => Err(ProtocolFailure::retryable(
                            ErrorCode::RecoveryRequired,
                            format!(
                                "cannot install snapshot: {install_error}; restore cleanup requires restart: {cleanup_error}"
                            ),
                        )),
                    }
                }
                Err(open_error) => Err(ProtocolFailure::retryable(
                    ErrorCode::RecoveryRequired,
                    format!(
                        "cannot install snapshot: {install_error}; restored live Database could not reopen: {open_error}"
                    ),
                )),
            };
        }
        sync_restore_directory(directory).map_err(internal)?;
        match EmbeddedDatabase::open(&handle.path) {
            Ok(database) => {
                *guard = Some(database);
                cleanup_committed_restore(directory, &backup, &journal, &candidate, &mut remove);
            }
            Err(error) => {
                if let Err(remove_error) = remove(&handle.path) {
                    return Err(ProtocolFailure::retryable(
                        ErrorCode::RecoveryRequired,
                        format!(
                            "installed snapshot could not reopen: {error}; cannot remove it for rollback: {remove_error}"
                        ),
                    ));
                }
                remove_sidecar(&handle.path);
                sync_restore_directory(directory).map_err(internal)?;
                if let Err(rollback_error) = rename(&backup, &handle.path) {
                    return Err(ProtocolFailure::retryable(
                        ErrorCode::RecoveryRequired,
                        format!(
                            "installed snapshot could not reopen: {error}; cannot restore the live Database: {rollback_error}"
                        ),
                    ));
                }
                sync_restore_directory(directory).map_err(internal)?;
                return match EmbeddedDatabase::open(&handle.path) {
                    Ok(database) => {
                        *guard = Some(database);
                        match cleanup_uncommitted_restore(
                            directory,
                            &candidate,
                            &journal,
                            &mut remove,
                        ) {
                            Ok(()) => Err(internal(format!(
                                "installed snapshot could not reopen: {error}"
                            ))),
                            Err(cleanup_error) => Err(ProtocolFailure::retryable(
                                ErrorCode::RecoveryRequired,
                                format!(
                                    "installed snapshot could not reopen: {error}; restore cleanup requires restart: {cleanup_error}"
                                ),
                            )),
                        }
                    }
                    Err(rollback_open_error) => Err(ProtocolFailure::retryable(
                        ErrorCode::RecoveryRequired,
                        format!(
                            "installed snapshot could not reopen: {error}; restored live Database could not reopen: {rollback_open_error}"
                        ),
                    )),
                };
            }
        }
        Ok(())
    })();
    if result.is_err() && candidate.exists() && !journal.exists() && remove(&candidate).is_ok() {
        remove_sidecar(&candidate);
        if let Err(error) = sync_restore_directory(directory) {
            tracing::warn!(%error, "restore candidate cleanup directory sync failed");
        }
    }
    result
}

fn cleanup_uncommitted_restore(
    directory: &Path,
    candidate: &Path,
    journal: &Path,
    remove: &mut impl FnMut(&Path) -> std::io::Result<()>,
) -> Result<(), ServerError> {
    remove_restore_file(candidate, remove)?;
    remove_sidecar(candidate);
    sync_restore_directory(directory)?;
    remove_restore_file(journal, remove)?;
    sync_restore_directory(directory)
}

fn cleanup_committed_restore(
    directory: &Path,
    backup: &Path,
    journal: &Path,
    candidate: &Path,
    remove: &mut impl FnMut(&Path) -> std::io::Result<()>,
) {
    if let Err(error) = remove_restore_file(backup, remove) {
        tracing::warn!(
            path = %backup.display(),
            %error,
            "committed snapshot backup cleanup failed; recovery journal retained"
        );
        return;
    }
    remove_sidecar(backup);
    if let Err(error) = sync_restore_directory(directory) {
        tracing::warn!(
            path = %directory.display(),
            %error,
            "committed snapshot cleanup directory sync failed; recovery journal retained"
        );
        return;
    }
    if let Err(error) = remove_restore_file(journal, remove) {
        tracing::warn!(
            path = %journal.display(),
            %error,
            "committed snapshot journal cleanup failed; startup will retry"
        );
    } else if let Err(error) = sync_restore_directory(directory) {
        tracing::warn!(
            path = %directory.display(),
            %error,
            "committed snapshot journal deletion directory sync failed"
        );
    }
    remove_sidecar(candidate);
}

fn remove_restore_file(
    path: &Path,
    remove: &mut impl FnMut(&Path) -> std::io::Result<()>,
) -> Result<(), ServerError> {
    match remove(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ServerError::new(format!(
            "cannot remove {}: {error}",
            path.display()
        ))),
    }
}

fn recover_snapshot_operations(root: &Path) -> Result<(), ServerError> {
    let databases = root.join("databases");
    if !databases.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(&databases)
        .map_err(|error| ServerError::new(format!("cannot inspect restore state: {error}")))?
    {
        let entry = entry.map_err(|error| ServerError::new(error.to_string()))?;
        if !entry.path().is_dir() {
            continue;
        }
        let directory = entry.path();
        let journal_path = directory.join("restore-operation");
        cleanup_unjournaled_restore_candidate(&directory)?;
        if !journal_path.exists() {
            continue;
        }
        let journal: RestoreJournal = serde_json::from_slice(
            &fs::read(&journal_path).map_err(|error| ServerError::new(error.to_string()))?,
        )
        .map_err(|error| ServerError::new(format!("invalid restore journal: {error}")))?;
        if journal.operation_version != 1
            || journal.database_id != entry.file_name().to_string_lossy()
        {
            return Err(ServerError::new(
                "restore journal identity or version is invalid; operator recovery is required",
            ));
        }
        let target = directory.join("database.ndb");
        let candidate = directory.join("database.ndb.restore-candidate");
        let backup = directory.join("database.ndb.restore-backup");
        if journal.stage != "prepared" {
            return Err(ServerError::new(
                "restore journal stage is invalid; operator recovery is required",
            ));
        }
        match (target.exists(), candidate.exists(), backup.exists()) {
            (true, true, false) => {
                remove_file_and_sidecar(&candidate)?;
            }
            (false, true, true) => {
                fs::rename(&candidate, &target).map_err(|error| {
                    ServerError::new(format!("cannot finish snapshot installation: {error}"))
                })?;
                sync_restore_directory(&directory)?;
                recover_or_accept_installed_snapshot(&target, &backup)?;
            }
            (true, false, true) => {
                recover_or_accept_installed_snapshot(&target, &backup)?;
            }
            (true, false, false) => {}
            (false, false, true) => {
                fs::rename(&backup, &target).map_err(|error| {
                    ServerError::new(format!("cannot roll back snapshot restore: {error}"))
                })?;
                sync_restore_directory(&directory)?;
            }
            _ => {
                return Err(ServerError::new(
                    "snapshot restore files are inconsistent; operator recovery is required",
                ));
            }
        }
        remove_file_and_sidecar(&candidate)?;
        remove_file_and_sidecar(&backup)?;
        sync_restore_directory(&directory)?;
        fs::remove_file(journal_path).map_err(|error| ServerError::new(error.to_string()))?;
        sync_restore_directory(&directory)?;
    }
    Ok(())
}

fn recover_or_accept_installed_snapshot(target: &Path, backup: &Path) -> Result<(), ServerError> {
    let validation_error = match validate_recovered_snapshot(target) {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };
    if let Err(remove_error) = fs::remove_file(target) {
        return Err(ServerError::new(format!(
            "installed snapshot is invalid ({validation_error}) and cannot be removed: {remove_error}; original backup and recovery journal were retained"
        )));
    }
    remove_sidecar(target);
    let directory = target
        .parent()
        .ok_or_else(|| ServerError::new("restored Database path has no parent"))?;
    sync_restore_directory(directory)?;
    fs::rename(backup, target).map_err(|rollback_error| {
        ServerError::new(format!(
            "installed snapshot is invalid ({validation_error}) and the original backup could not be restored: {rollback_error}"
        ))
    })?;
    sync_restore_directory(directory)?;
    validate_recovered_snapshot(target).map_err(|rollback_error| {
        ServerError::new(format!(
            "the installed snapshot was invalid ({validation_error}) and the restored original Database is also invalid: {rollback_error}"
        ))
    })
}

fn validate_recovered_snapshot(path: &Path) -> Result<(), ServerError> {
    let database = EmbeddedDatabase::open(path).map_err(|error| {
        ServerError::new(format!(
            "cannot open restored Database {}: {error}",
            path.display()
        ))
    })?;
    let report = database.check().map_err(|error| {
        ServerError::new(format!(
            "cannot check restored Database {}: {error}",
            path.display()
        ))
    })?;
    if report.is_valid() {
        Ok(())
    } else {
        Err(ServerError::new(format!(
            "restored Database {} failed integrity validation",
            path.display()
        )))
    }
}

fn cleanup_unjournaled_restore_candidate(directory: &Path) -> Result<(), ServerError> {
    let target = directory.join("database.ndb");
    let candidate = directory.join("database.ndb.restore-candidate");
    let backup = directory.join("database.ndb.restore-backup");
    let journal = directory.join("restore-operation");
    if journal.exists() || (!candidate.exists() && !backup.exists()) {
        return Ok(());
    }
    if target.exists() && candidate.exists() && !backup.exists() {
        remove_file_and_sidecar(&candidate)?;
        return sync_restore_directory(directory);
    }
    Err(ServerError::new(
        "snapshot restore files exist without a journal; operator recovery is required",
    ))
}

fn import_logical(
    handle: &ManagedDatabase,
    package: LogicalPackageDocument,
) -> Result<(), ProtocolFailure> {
    if package.package_version != 1 || package.language_version != 1 {
        return Err(ProtocolFailure::new(
            ErrorCode::QueryError,
            "logical package and language versions must both be 1",
        ));
    }
    let parent = handle
        .path
        .parent()
        .ok_or_else(|| internal("managed Database path has no parent"))?;
    let source_root = parent.join(format!("logical-import-{}", Uuid::new_v4()));
    fs::create_dir(&source_root).map_err(internal)?;
    let result = (|| {
        fs::write(source_root.join("nostdb.toml"), package.config).map_err(internal)?;
        let config = nostdb_engine::ProjectConfig::load(&source_root)
            .map_err(|error| ProtocolFailure::new(ErrorCode::QueryError, error.to_string()))?;
        let mut seen = std::collections::BTreeSet::new();
        for module in package.modules {
            let relative = safe_module_path(&module.path)?;
            if !seen.insert(relative.clone()) {
                return Err(ProtocolFailure::new(
                    ErrorCode::QueryError,
                    "logical package repeats a module path",
                ));
            }
            let module_id = module
                .stable_module_id
                .parse::<nostdb_engine::StableModuleId>()
                .map_err(|_| {
                    ProtocolFailure::new(ErrorCode::QueryError, "invalid stable Module ID")
                })?;
            if config.module_id(&relative) != Some(module_id) {
                return Err(ProtocolFailure::new(
                    ErrorCode::QueryError,
                    format!(
                        "stable Module ID does not match nostdb.toml for {}",
                        module.path
                    ),
                ));
            }
            let path = source_root.join(&relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(internal)?;
            }
            fs::write(path, module.source).map_err(internal)?;
        }
        let candidate = source_root.join("candidate.ndb");
        nostdb_engine::Synchronizer::default()
            .sync(&source_root, &candidate)
            .map_err(|error| ProtocolFailure::new(ErrorCode::QueryError, error.to_string()))?;
        let bytes = fs::read(&candidate).map_err(internal)?;
        restore_snapshot(handle, &bytes)
    })();
    let cleanup = fs::remove_dir_all(&source_root);
    preserve_protocol_result_after_cleanup(result, &source_root, cleanup)
}

fn preserve_protocol_result_after_cleanup<T>(
    result: Result<T, ProtocolFailure>,
    path: &Path,
    cleanup: std::io::Result<()>,
) -> Result<T, ProtocolFailure> {
    if let Err(error) = cleanup {
        tracing::warn!(
            path = %path.display(),
            operation_succeeded = result.is_ok(),
            %error,
            "logical import temporary-directory cleanup failed"
        );
    }
    result
}

fn preserve_committed_lifecycle_result<T>(
    result: T,
    operation: &str,
    cleanup: Result<(), ServerError>,
) -> T {
    if let Err(error) = cleanup {
        tracing::warn!(
            operation,
            %error,
            "committed Database lifecycle journal cleanup failed; startup will retry"
        );
    }
    result
}

fn safe_module_path(value: &str) -> Result<PathBuf, ProtocolFailure> {
    use std::path::Component;
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || path.extension().and_then(|extension| extension.to_str()) != Some("nostdb")
    {
        return Err(ProtocolFailure::new(
            ErrorCode::QueryError,
            format!("invalid logical module path `{value}`"),
        ));
    }
    Ok(path.to_path_buf())
}

fn write_new_synced(path: &Path, bytes: &[u8]) -> Result<(), ProtocolFailure> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(internal)?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        drop(file);
        return match fs::remove_file(path) {
            Ok(()) => Err(internal(format!(
                "cannot persist {}: {error}",
                path.display()
            ))),
            Err(cleanup_error) if cleanup_error.kind() == std::io::ErrorKind::NotFound => Err(
                internal(format!("cannot persist {}: {error}", path.display())),
            ),
            Err(cleanup_error) => Err(internal(format!(
                "cannot persist {}: {error}; cannot remove partial file: {cleanup_error}",
                path.display()
            ))),
        };
    }
    Ok(())
}

fn write_restore_journal(path: &Path, id: &str, stage: &str) -> Result<(), ProtocolFailure> {
    let journal = RestoreJournal {
        operation_version: 1,
        database_id: id.to_owned(),
        stage: stage.to_owned(),
    };
    let bytes = serde_json::to_vec_pretty(&journal).map_err(internal)?;
    write_new_synced(path, &bytes)
}

#[cfg(unix)]
fn sync_restore_directory(directory: &Path) -> Result<(), ServerError> {
    File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(|error| {
            ServerError::new(format!(
                "cannot sync restore directory {}: {error}",
                directory.display()
            ))
        })
}

#[cfg(not(unix))]
fn sync_restore_directory(directory: &Path) -> Result<(), ServerError> {
    let _ = directory;
    Ok(())
}

fn remove_file_and_sidecar(path: &Path) -> Result<(), ServerError> {
    if path.exists() {
        fs::remove_file(path).map_err(|error| ServerError::new(error.to_string()))?;
    }
    remove_sidecar(path);
    Ok(())
}

fn remove_sidecar(path: &Path) {
    let mut value = path.as_os_str().to_os_string();
    value.push(".lock");
    let _ = fs::remove_file(PathBuf::from(value));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "nostdb-daemon-{name}-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ))
    }

    fn snapshot_bytes(root: &Path, name: &str) -> Vec<u8> {
        let path = root.join(name);
        let mut database = EmbeddedDatabase::create(&path).expect("snapshot Database creates");
        database
            .adopt_server_authority()
            .expect("snapshot adopts Server authority");
        database.checkpoint().expect("snapshot checkpoints");
        drop(database);
        fs::read(path).expect("snapshot bytes read")
    }

    #[test]
    fn initialization_checks_the_config_before_mutating_an_empty_data_directory() {
        let root = test_root("init-existing-config");
        let data_directory = root.join("data");
        let config_path = root.join("server.toml");
        fs::create_dir_all(&data_directory).expect("empty data directory creates");
        fs::write(&config_path, "operator-owned sentinel\n").expect("sentinel config writes");

        let error = DatabaseDaemon::initialize(&config_path, &data_directory, "127.0.0.1:0")
            .expect_err("existing config rejects initialization");
        assert!(error.to_string().contains("already exists"));
        assert_eq!(
            fs::read_to_string(&config_path).expect("sentinel config reads"),
            "operator-owned sentinel\n"
        );
        assert!(data_directory.is_dir());
        assert_eq!(
            fs::read_dir(&data_directory)
                .expect("data directory reads")
                .count(),
            0
        );

        fs::remove_file(&config_path).expect("sentinel config removes");
        DatabaseDaemon::initialize(&config_path, &data_directory, "127.0.0.1:0")
            .expect("same data directory can be retried");
        assert!(data_directory.join("server-state").is_file());
        fs::remove_dir_all(root).expect("test directory removes");
    }

    #[test]
    fn initialization_rolls_back_its_layout_after_a_late_config_failure() {
        let root = test_root("init-config-rollback");
        let data_directory = root.join("data");
        let conflicting_config_path = data_directory.join("server-state");
        fs::create_dir(&root).expect("test directory creates");

        let error =
            DatabaseDaemon::initialize(&conflicting_config_path, &data_directory, "127.0.0.1:0")
                .expect_err("catalog/config path collision rejects initialization");
        assert!(error.to_string().contains("cannot create"));
        assert!(
            !data_directory.exists(),
            "only the failed initialization created the data directory"
        );

        let config_path = root.join("server.toml");
        DatabaseDaemon::initialize(&config_path, &data_directory, "127.0.0.1:0")
            .expect("clean retry succeeds after rollback");
        assert!(data_directory.join("server-state").is_file());
        fs::remove_dir_all(root).expect("test directory removes");
    }

    #[test]
    fn failed_drop_checkpoint_restores_the_live_database_handle() {
        let root = test_root("drop-checkpoint-failure");
        let data_directory = root.join("data");
        let config_path = root.join("server.toml");
        fs::create_dir(&root).expect("test directory creates");
        DatabaseDaemon::initialize(&config_path, &data_directory, "127.0.0.1:0")
            .expect("daemon initializes");
        let daemon = DatabaseDaemon::open(
            DaemonConfig::load(&config_path).expect("daemon configuration loads"),
        )
        .expect("daemon opens");
        daemon
            .create_database("knowledge")
            .expect("Database creates");
        let injected =
            ProtocolFailure::retryable(ErrorCode::DatabaseBusy, "injected checkpoint failure");

        let error = daemon
            .drop_database_with_checkpoint("knowledge", "knowledge", |_| Err(injected.clone()))
            .expect_err("checkpoint failure rejects drop");

        assert_eq!(error, injected);
        assert_eq!(
            daemon
                .inspect_database("knowledge")
                .expect("Database remains immediately usable")
                .summary
                .name,
            "knowledge"
        );
        daemon
            .drop_database("knowledge", "knowledge")
            .expect("a later drop succeeds without restart");
        drop(daemon);
        fs::remove_dir_all(root).expect("test directory removes");
    }

    #[test]
    fn drop_preflights_a_conflicting_trash_target_before_closing_the_database() {
        let root = test_root("drop-trash-preflight");
        let data_directory = root.join("data");
        let config_path = root.join("server.toml");
        fs::create_dir(&root).expect("test directory creates");
        DatabaseDaemon::initialize(&config_path, &data_directory, "127.0.0.1:0")
            .expect("daemon initializes");
        let daemon = DatabaseDaemon::open(
            DaemonConfig::load(&config_path).expect("daemon configuration loads"),
        )
        .expect("daemon opens");
        let created = daemon
            .create_database("knowledge")
            .expect("Database creates");
        let trash = data_directory.join("trash").join(created.id);
        fs::create_dir(&trash).expect("conflicting trash target creates");

        daemon
            .drop_database("knowledge", "knowledge")
            .expect_err("conflicting trash target rejects drop");

        daemon
            .inspect_database("knowledge")
            .expect("Database remains immediately usable");
        fs::remove_dir(&trash).expect("conflicting trash target removes");
        daemon
            .drop_database("knowledge", "knowledge")
            .expect("drop succeeds after conflict is removed");
        drop(daemon);
        fs::remove_dir_all(root).expect("test directory removes");
    }

    #[test]
    fn lifecycle_cleanup_failure_does_not_turn_committed_success_into_failure() {
        let result = preserve_committed_lifecycle_result(
            7_u8,
            "injected",
            Err(ServerError::new("injected journal cleanup failure")),
        );

        assert_eq!(result, 7);
    }

    #[test]
    fn startup_recovery_removes_an_unjournaled_restore_candidate() {
        let root = test_root("restore-orphan-candidate");
        let directory = root.join("databases/database-id");
        let target = directory.join("database.ndb");
        let candidate = directory.join("database.ndb.restore-candidate");
        let mut sidecar = candidate.as_os_str().to_os_string();
        sidecar.push(".lock");
        let sidecar = PathBuf::from(sidecar);
        fs::create_dir_all(&directory).expect("Database directory creates");
        fs::write(&target, b"live Database").expect("live Database writes");
        fs::write(&candidate, b"partial candidate").expect("candidate writes");
        fs::write(&sidecar, b"stale sidecar").expect("candidate sidecar writes");

        recover_snapshot_operations(&root).expect("startup recovery succeeds");

        assert_eq!(
            fs::read(&target).expect("live Database reads"),
            b"live Database"
        );
        assert!(!candidate.exists());
        assert!(!sidecar.exists());
        fs::remove_dir_all(root).expect("test directory removes");
    }

    #[cfg(unix)]
    #[test]
    fn restore_directory_sync_contract_accepts_directory_metadata_barriers() {
        let root = test_root("restore-directory-sync");
        fs::create_dir(&root).expect("test directory creates");
        fs::write(root.join("entry"), b"durability barrier").expect("entry writes");

        sync_restore_directory(&root).expect("created directory entry syncs");
        fs::remove_file(root.join("entry")).expect("entry removes");
        sync_restore_directory(&root).expect("removed directory entry syncs");

        fs::remove_dir(root).expect("test directory removes");
    }

    #[test]
    fn restore_checkpoint_failure_restores_handle_and_removes_prepared_state() {
        let root = test_root("restore-checkpoint-failure");
        let directory = root.join("databases/database-id");
        fs::create_dir_all(&directory).expect("Database directory creates");
        let path = directory.join("database.ndb");
        let database = EmbeddedDatabase::create(&path).expect("live Database creates");
        let handle = ManagedDatabase::new("database-id".to_owned(), path, database);
        let bytes = snapshot_bytes(&root, "snapshot.ndb");
        let injected =
            ProtocolFailure::retryable(ErrorCode::DatabaseBusy, "injected live checkpoint failure");

        let error = restore_snapshot_with_operations(
            &handle,
            &bytes,
            |_| Err(injected.clone()),
            |from, to| fs::rename(from, to),
            |path| fs::remove_file(path),
        )
        .expect_err("checkpoint failure rejects restore");

        assert_eq!(error, injected);
        assert!(
            handle
                .database
                .lock()
                .expect("Database handle locks")
                .is_some(),
            "live Database is immediately restored to the handle"
        );
        assert!(!directory.join("database.ndb.restore-candidate").exists());
        assert!(!directory.join("restore-operation").exists());
        drop(handle);
        fs::remove_dir_all(root).expect("test directory removes");
    }

    #[test]
    fn committed_restore_cleanup_failure_preserves_success_and_recovery_evidence() {
        let root = test_root("restore-committed-cleanup-failure");
        let directory = root.join("databases/database-id");
        fs::create_dir_all(&directory).expect("Database directory creates");
        let path = directory.join("database.ndb");
        let backup = directory.join("database.ndb.restore-backup");
        let journal = directory.join("restore-operation");
        let database = EmbeddedDatabase::create(&path).expect("live Database creates");
        let handle = ManagedDatabase::new("database-id".to_owned(), path, database);
        let bytes = snapshot_bytes(&root, "snapshot.ndb");

        restore_snapshot_with_operations(
            &handle,
            &bytes,
            |database| {
                database
                    .checkpoint()
                    .map_err(|error| database_failure(error, None))
            },
            |from, to| fs::rename(from, to),
            |path| {
                if path == backup {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "injected committed cleanup failure",
                    ))
                } else {
                    fs::remove_file(path)
                }
            },
        )
        .expect("committed restore remains successful");

        assert!(backup.is_file());
        assert!(journal.is_file(), "journal is retained with the backup");
        assert!(
            handle
                .database
                .lock()
                .expect("Database handle locks")
                .is_some()
        );
        recover_snapshot_operations(&root).expect("startup cleanup can finish committed restore");
        assert!(!backup.exists());
        assert!(!journal.exists());
        drop(handle);
        fs::remove_dir_all(root).expect("test directory removes");
    }

    #[test]
    fn startup_recovery_rolls_an_invalid_installed_snapshot_back_to_the_backup() {
        let root = test_root("restore-invalid-installed-target");
        let directory = root.join("databases/database-id");
        let target = directory.join("database.ndb");
        let backup = directory.join("database.ndb.restore-backup");
        let journal = directory.join("restore-operation");
        fs::create_dir_all(&directory).expect("Database directory creates");
        let mut original = EmbeddedDatabase::create(&backup).expect("backup Database creates");
        original.checkpoint().expect("backup Database checkpoints");
        drop(original);
        fs::write(&target, b"invalid installed snapshot").expect("invalid target writes");
        write_restore_journal(&journal, "database-id", "prepared").expect("restore journal writes");

        recover_snapshot_operations(&root).expect("invalid target rolls back");

        EmbeddedDatabase::open(&target).expect("restored original Database opens");
        assert!(!backup.exists());
        assert!(!journal.exists());
        fs::remove_dir_all(root).expect("test directory removes");
    }

    #[test]
    fn startup_recovery_retains_backup_when_an_invalid_target_cannot_be_removed() {
        let root = test_root("restore-unremovable-installed-target");
        let directory = root.join("databases/database-id");
        let target = directory.join("database.ndb");
        let backup = directory.join("database.ndb.restore-backup");
        let journal = directory.join("restore-operation");
        fs::create_dir_all(&target).expect("invalid target directory creates");
        let mut original = EmbeddedDatabase::create(&backup).expect("backup Database creates");
        original.checkpoint().expect("backup Database checkpoints");
        drop(original);
        write_restore_journal(&journal, "database-id", "prepared").expect("restore journal writes");

        let error = recover_snapshot_operations(&root)
            .expect_err("unremovable invalid target requires recovery");

        assert!(error.to_string().contains("original backup"));
        assert!(target.is_dir());
        assert!(backup.is_file(), "original backup must be retained");
        assert!(journal.is_file(), "recovery journal must be retained");
        fs::remove_dir_all(root).expect("test directory removes");
    }

    #[test]
    fn failed_snapshot_install_and_rollback_retain_the_recovery_journal() {
        let root = test_root("restore-install-rollback-failure");
        let directory = root.join("databases/database-id");
        fs::create_dir_all(&directory).expect("Database directory creates");
        let path = directory.join("database.ndb");
        let candidate = directory.join("database.ndb.restore-candidate");
        let backup = directory.join("database.ndb.restore-backup");
        let journal = directory.join("restore-operation");
        let database = EmbeddedDatabase::create(&path).expect("live Database creates");
        let handle = ManagedDatabase::new("database-id".to_owned(), path.clone(), database);
        let bytes = snapshot_bytes(&root, "snapshot.ndb");
        let mut rename_calls = 0_u8;

        let error = restore_snapshot_with_operations(
            &handle,
            &bytes,
            |database| {
                database
                    .checkpoint()
                    .map_err(|error| database_failure(error, None))
            },
            |from, to| {
                rename_calls += 1;
                if rename_calls == 1 {
                    fs::rename(from, to)
                } else {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "injected rename failure",
                    ))
                }
            },
            |path| fs::remove_file(path),
        )
        .expect_err("install and rollback failure rejects restore");

        assert_eq!(error.code, ErrorCode::RecoveryRequired);
        assert!(!path.exists());
        assert!(candidate.is_file());
        assert!(backup.is_file());
        assert!(
            journal.is_file(),
            "journal must survive until restart recovery"
        );
        drop(handle);
        fs::remove_dir_all(root).expect("test directory removes");
    }

    #[test]
    fn failed_logical_import_removes_its_temporary_directory() {
        let root = test_root("logical-import-cleanup");
        let path = root.join("database.ndb");
        fs::create_dir(&root).expect("test directory creates");
        let database = EmbeddedDatabase::create(&path).expect("Database creates");
        let handle = ManagedDatabase::new("database-id".to_owned(), path, database);
        let package = LogicalPackageDocument {
            package_version: 1,
            language_version: 1,
            config: "this is not valid TOML =".to_owned(),
            modules: Vec::new(),
        };

        import_logical(&handle, package).expect_err("invalid package rejects");

        assert!(
            fs::read_dir(&root)
                .expect("test directory reads")
                .all(|entry| !entry
                    .expect("directory entry reads")
                    .file_name()
                    .to_string_lossy()
                    .starts_with("logical-import-"))
        );
        drop(handle);
        fs::remove_dir_all(root).expect("test directory removes");
    }

    #[test]
    fn post_commit_cleanup_failure_does_not_turn_success_into_failure() {
        let result: Result<u8, ProtocolFailure> = preserve_protocol_result_after_cleanup(
            Ok(7),
            Path::new("injected-logical-import-directory"),
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected cleanup failure",
            )),
        );

        assert_eq!(result.expect("operation result is preserved"), 7);
    }

    #[test]
    fn client_limits_can_only_lower_server_limits() {
        let defaults = QueryLimits {
            max_rows: 10,
            max_memory_bytes: 20,
            max_operations: 30,
            max_traversals: 40,
        };
        let lowered = lower_limits(
            defaults,
            Some(WireQueryLimits {
                max_rows: Some(5),
                max_memory_bytes: Some(200),
                max_operations: None,
                max_traversals: Some(0),
            }),
        );
        assert_eq!(lowered.max_rows, 5);
        assert_eq!(lowered.max_memory_bytes, 20);
        assert_eq!(lowered.max_operations, 30);
        assert_eq!(lowered.max_traversals, 0);
    }
}
