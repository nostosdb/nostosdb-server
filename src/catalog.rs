use std::collections::BTreeSet;
#[cfg(unix)]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use nostdb_client::DatabaseSummary;
use serde::{Deserialize, Serialize};

use crate::ServerError;

pub(crate) const CATALOG_VERSION: u32 = 1;
const OPERATION_VERSION: u32 = 1;
const STATE_FILE: &str = "server-state";
const PENDING_FILE: &str = "server-state.pending";
const PREVIOUS_FILE: &str = "server-state.previous";
const OPERATION_FILE: &str = "server-operation";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Catalog {
    pub(crate) catalog_version: u32,
    pub(crate) databases: Vec<CatalogDatabase>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CatalogDatabase {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) state: String,
}

impl CatalogDatabase {
    pub(crate) fn summary(&self) -> DatabaseSummary {
        DatabaseSummary {
            id: self.id.clone(),
            name: self.name.clone(),
            state: self.state.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OperationKind {
    Create,
    Rename,
    Drop,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationJournal {
    operation_version: u32,
    kind: OperationKind,
    database_id: String,
    before: Catalog,
    after: Catalog,
}

pub(crate) struct CatalogStore {
    root: PathBuf,
    catalog: Catalog,
}

impl CatalogStore {
    pub(crate) fn initialize(root: &Path) -> Result<Self, ServerError> {
        let root_existed = root.exists();
        if root_existed {
            let mut entries = fs::read_dir(root).map_err(|error| {
                ServerError::new(format!(
                    "cannot inspect data directory {}: {error}",
                    root.display()
                ))
            })?;
            if entries
                .next()
                .transpose()
                .map_err(|error| {
                    ServerError::new(format!(
                        "cannot inspect data directory {}: {error}",
                        root.display()
                    ))
                })?
                .is_some()
            {
                return Err(ServerError::new(format!(
                    "data directory {} is not empty; init never adopts existing files",
                    root.display()
                )));
            }
        } else if let Err(error) = fs::create_dir_all(root) {
            let error = ServerError::new(format!(
                "cannot create data directory {}: {error}",
                root.display()
            ));
            return match rollback_created_directories(root, root_existed, &[]) {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(ServerError::new(format!(
                    "{error}; catalog initialization rollback was incomplete: {rollback_error}"
                ))),
            };
        }
        let mut created_directories = Vec::new();
        let result = (|| {
            for directory in ["databases", "snapshots", "locks", "recovery", "trash"] {
                let path = root.join(directory);
                fs::create_dir(&path).map_err(|error| {
                    ServerError::new(format!("cannot create data layout: {error}"))
                })?;
                created_directories.push(path);
            }
            let catalog = Catalog {
                catalog_version: CATALOG_VERSION,
                databases: Vec::new(),
            };
            write_atomic(root, &catalog, true)?;
            Ok(Self {
                root: root.to_path_buf(),
                catalog,
            })
        })();
        match result {
            Ok(store) => Ok(store),
            Err(error) => {
                match rollback_created_directories(root, root_existed, &created_directories) {
                    Ok(()) => Err(error),
                    Err(rollback_error) => Err(ServerError::new(format!(
                        "{error}; catalog initialization rollback was incomplete: {rollback_error}"
                    ))),
                }
            }
        }
    }

    pub(crate) fn rollback_initialization(
        root: &Path,
        root_existed: bool,
    ) -> Result<(), ServerError> {
        let mut failures = Vec::new();
        for file in [PENDING_FILE, PREVIOUS_FILE, OPERATION_FILE, STATE_FILE] {
            let path = root.join(file);
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    failures.push(format!("cannot remove {}: {error}", path.display()));
                }
            }
        }
        for directory in ["trash", "recovery", "locks", "snapshots", "databases"] {
            let path = root.join(directory);
            match fs::remove_dir(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    failures.push(format!("cannot remove {}: {error}", path.display()));
                }
            }
        }
        if !root_existed {
            match fs::remove_dir(root) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    failures.push(format!("cannot remove {}: {error}", root.display()));
                }
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(ServerError::new(failures.join("; ")))
        }
    }

    pub(crate) fn load(root: &Path) -> Result<Self, ServerError> {
        if !root.is_dir() {
            return Err(ServerError::new(format!(
                "data directory {} does not exist or is not a directory; run `nostd init` first",
                root.display()
            )));
        }
        recover_atomic_state(root)?;
        let mut catalog = read_catalog(&root.join(STATE_FILE))?;
        validate_catalog(&catalog)?;
        recover_operation(root, &mut catalog)?;
        validate_storage(root, &catalog)?;
        recover_orphans(root, &catalog)?;
        Ok(Self {
            root: root.to_path_buf(),
            catalog,
        })
    }

    pub(crate) fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub(crate) fn database_path(&self, id: &str) -> PathBuf {
        database_path(&self.root, id)
    }

    pub(crate) fn database_directory(&self, id: &str) -> PathBuf {
        self.root.join("databases").join(id)
    }

    pub(crate) fn trash_directory(&self, id: &str) -> PathBuf {
        self.root.join("trash").join(id)
    }

    pub(crate) fn transition(
        &mut self,
        next: Catalog,
        kind: OperationKind,
        database_id: &str,
    ) -> Result<(), ServerError> {
        validate_catalog(&next)?;
        let journal = OperationJournal {
            operation_version: OPERATION_VERSION,
            kind,
            database_id: database_id.to_owned(),
            before: self.catalog.clone(),
            after: next.clone(),
        };
        write_json_new(&self.root.join(OPERATION_FILE), &journal)?;
        if let Err(error) = write_atomic(&self.root, &next, false) {
            let _ = fs::remove_file(self.root.join(OPERATION_FILE));
            return Err(error);
        }
        self.catalog = next;
        Ok(())
    }

    pub(crate) fn finish_transition(&self) -> Result<(), ServerError> {
        remove_if_exists(&self.root.join(OPERATION_FILE))
    }
}

pub(crate) fn valid_database_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 63
        && bytes[0].is_ascii_lowercase()
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

fn validate_catalog(catalog: &Catalog) -> Result<(), ServerError> {
    if catalog.catalog_version != CATALOG_VERSION {
        return Err(ServerError::new(format!(
            "unsupported catalog_version {}; this binary supports exactly {CATALOG_VERSION}",
            catalog.catalog_version
        )));
    }
    let mut ids = BTreeSet::new();
    let mut names = BTreeSet::new();
    for database in &catalog.databases {
        if uuid::Uuid::parse_str(&database.id).is_err() {
            return Err(ServerError::new(format!(
                "catalog contains invalid stable Database ID `{}`",
                database.id
            )));
        }
        if !valid_database_name(&database.name) {
            return Err(ServerError::new(format!(
                "catalog contains invalid Database name `{}`",
                database.name
            )));
        }
        if database.state != "ready" {
            return Err(ServerError::new(format!(
                "catalog Database `{}` has unsupported state `{}`",
                database.name, database.state
            )));
        }
        if !ids.insert(database.id.clone()) || !names.insert(database.name.clone()) {
            return Err(ServerError::new(
                "catalog contains duplicate Database identity or name",
            ));
        }
    }
    if !catalog
        .databases
        .windows(2)
        .all(|pair| pair[0].name < pair[1].name)
    {
        return Err(ServerError::new(
            "catalog Databases must be sorted by unique name",
        ));
    }
    Ok(())
}

fn validate_storage(root: &Path, catalog: &Catalog) -> Result<(), ServerError> {
    for database in &catalog.databases {
        let directory = root.join("databases").join(&database.id);
        let path = directory.join("database.ndb");
        if !directory.is_dir() || !path.is_file() {
            return Err(ServerError::new(format!(
                "catalog Database `{}` has missing managed storage; recovery is required",
                database.name
            )));
        }
    }
    Ok(())
}

fn recover_orphans(root: &Path, catalog: &Catalog) -> Result<(), ServerError> {
    let known = catalog
        .databases
        .iter()
        .map(|database| database.id.as_str())
        .collect::<BTreeSet<_>>();
    for entry in fs::read_dir(root.join("databases"))
        .map_err(|error| ServerError::new(format!("cannot inspect managed Databases: {error}")))?
    {
        let entry = entry.map_err(|error| ServerError::new(error.to_string()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if known.contains(name.as_str()) {
            continue;
        }
        let target = root.join("recovery").join(format!("orphan-{name}"));
        if target.exists() {
            return Err(ServerError::new(format!(
                "orphan storage `{name}` and its recovery target both exist; operator recovery is required"
            )));
        }
        fs::rename(entry.path(), target).map_err(|error| {
            ServerError::new(format!(
                "cannot quarantine orphan storage `{name}`: {error}"
            ))
        })?;
    }
    Ok(())
}

fn recover_atomic_state(root: &Path) -> Result<(), ServerError> {
    let state = root.join(STATE_FILE);
    let pending = root.join(PENDING_FILE);
    let previous = root.join(PREVIOUS_FILE);
    if !state.exists() && previous.exists() {
        fs::rename(&previous, &state).map_err(|error| {
            ServerError::new(format!("cannot restore previous catalog: {error}"))
        })?;
    }
    if pending.exists() {
        let target = root.join("recovery").join("interrupted-catalog.pending");
        if target.exists() {
            return Err(ServerError::new(
                "multiple interrupted catalog writes require operator recovery",
            ));
        }
        fs::rename(&pending, target).map_err(|error| {
            ServerError::new(format!("cannot preserve interrupted catalog: {error}"))
        })?;
    }
    if state.exists() && previous.exists() {
        read_catalog(&state)?;
        remove_if_exists(&previous)?;
    }
    if !state.exists() {
        return Err(ServerError::new(
            "data directory has no server-state catalog; run `nostd init` for a fresh directory",
        ));
    }
    Ok(())
}

fn recover_operation(root: &Path, catalog: &mut Catalog) -> Result<(), ServerError> {
    let path = root.join(OPERATION_FILE);
    if !path.exists() {
        return Ok(());
    }
    let bytes = fs::read(&path)
        .map_err(|error| ServerError::new(format!("cannot read lifecycle journal: {error}")))?;
    let journal: OperationJournal = serde_json::from_slice(&bytes)
        .map_err(|error| ServerError::new(format!("invalid lifecycle journal: {error}")))?;
    if journal.operation_version != OPERATION_VERSION {
        return Err(ServerError::new(format!(
            "unsupported lifecycle operation_version {}",
            journal.operation_version
        )));
    }
    if *catalog == journal.before {
        remove_if_exists(&path)?;
        return Ok(());
    }
    if *catalog != journal.after {
        return Err(ServerError::new(
            "catalog matches neither side of the lifecycle journal; operator recovery is required",
        ));
    }
    match journal.kind {
        OperationKind::Create => {
            if !database_path(root, &journal.database_id).is_file() {
                return Err(ServerError::new(
                    "committed Database create has no storage; operator recovery is required",
                ));
            }
        }
        OperationKind::Rename => {}
        OperationKind::Drop => {
            let source = root.join("databases").join(&journal.database_id);
            if source.exists() {
                let target = root.join("trash").join(&journal.database_id);
                if target.exists() {
                    return Err(ServerError::new(
                        "drop recovery found both active and trash storage",
                    ));
                }
                fs::rename(source, target).map_err(|error| {
                    ServerError::new(format!("cannot finish interrupted Database drop: {error}"))
                })?;
            }
        }
    }
    remove_if_exists(&path)
}

fn write_atomic(root: &Path, catalog: &Catalog, create: bool) -> Result<(), ServerError> {
    let state = root.join(STATE_FILE);
    let pending = root.join(PENDING_FILE);
    let previous = root.join(PREVIOUS_FILE);
    write_json_new(&pending, catalog)?;
    if create {
        if state.exists() {
            let _ = fs::remove_file(&pending);
            return Err(ServerError::new("server-state already exists"));
        }
    } else {
        remove_if_exists(&previous)?;
        fs::rename(&state, &previous).map_err(|error| {
            ServerError::new(format!("cannot preserve previous catalog: {error}"))
        })?;
    }
    if let Err(error) = fs::rename(&pending, &state) {
        let _ = fs::remove_file(&pending);
        if !create {
            let _ = fs::rename(&previous, &state);
        }
        return Err(ServerError::new(format!("cannot install catalog: {error}")));
    }
    if let Err(error) = sync_parent(root) {
        let _ = fs::remove_file(&state);
        if !create {
            let _ = fs::rename(&previous, &state);
            let _ = sync_parent(root);
        }
        return Err(error);
    }
    let _ = remove_if_exists(&previous);
    Ok(())
}

fn write_json_new(path: &Path, value: &impl Serialize) -> Result<(), ServerError> {
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| ServerError::new(format!("cannot encode durable state: {error}")))?;
    bytes.push(b'\n');
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|error| ServerError::new(format!("cannot create {}: {error}", path.display())))?;
    if let Err(error) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
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

fn rollback_created_directories(
    root: &Path,
    root_existed: bool,
    created_directories: &[PathBuf],
) -> Result<(), ServerError> {
    let mut failures = Vec::new();
    for path in created_directories.iter().rev() {
        match fs::remove_dir(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => failures.push(format!("cannot remove {}: {error}", path.display())),
        }
    }
    if !root_existed {
        match fs::remove_dir(root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => failures.push(format!("cannot remove {}: {error}", root.display())),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(ServerError::new(failures.join("; ")))
    }
}

fn read_catalog(path: &Path) -> Result<Catalog, ServerError> {
    let bytes = fs::read(path).map_err(|error| {
        ServerError::new(format!("cannot read catalog {}: {error}", path.display()))
    })?;
    serde_json::from_slice(&bytes)
        .map_err(|error| ServerError::new(format!("invalid catalog {}: {error}", path.display())))
}

fn database_path(root: &Path, id: &str) -> PathBuf {
    root.join("databases").join(id).join("database.ndb")
}

fn remove_if_exists(path: &Path) -> Result<(), ServerError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ServerError::new(format!(
            "cannot remove {}: {error}",
            path.display()
        ))),
    }
}

#[cfg(unix)]
fn sync_parent(root: &Path) -> Result<(), ServerError> {
    File::open(root)
        .and_then(|file| file.sync_all())
        .map_err(|error| ServerError::new(format!("cannot sync data directory: {error}")))
}

#[cfg(not(unix))]
fn sync_parent(_root: &Path) -> Result<(), ServerError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "nostdb-catalog-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ))
    }

    #[test]
    fn names_are_portable_and_path_independent() {
        for valid in ["a", "knowledge", "team_graph-2"] {
            assert!(valid_database_name(valid), "{valid}");
        }
        for invalid in ["", "Upper", "2first", "../escape", "a.b", "한글"] {
            assert!(!valid_database_name(invalid), "{invalid}");
        }
    }

    #[test]
    fn interrupted_catalog_write_preserves_pending_evidence() {
        let root = root("pending");
        let store = CatalogStore::initialize(&root).expect("catalog initializes");
        fs::write(root.join(PENDING_FILE), b"partial").expect("pending state writes");
        drop(store);
        CatalogStore::load(&root).expect("catalog recovers");
        assert!(root.join("recovery/interrupted-catalog.pending").exists());
        fs::remove_dir_all(root).expect("test directory removes");
    }
}
