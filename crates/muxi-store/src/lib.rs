//! SQLite-backed event log and content-addressed storage primitives.

use std::path::{Path, PathBuf};

use blake3::Hash;
use muxi_core::{AppState, DomainEvent, StateError, reduce};
use rusqlite::{Connection, params};
use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;

pub const CRATE_NAME: &str = "muxi-store";

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("state error: {0}")]
    State(#[from] StateError),
    #[error("CAS I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub struct EventStore {
    connection: Connection,
}

impl EventStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "foreign_keys", true)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS events (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                event_type TEXT NOT NULL,
                payload TEXT NOT NULL
            );",
        )?;
        Ok(Self { connection })
    }

    pub fn append(&self, event: &DomainEvent) -> Result<i64, StoreError> {
        let payload = serde_json::to_string(event)?;
        let event_type = match event {
            DomainEvent::TaskCreated { .. } => "task_created",
            DomainEvent::PhaseChanged { .. } => "phase_changed",
            DomainEvent::TaskRenamed { .. } => "task_renamed",
            DomainEvent::RecoveryRequired { .. } => "recovery_required",
        };
        self.connection.execute(
            "INSERT INTO events (event_type, payload) VALUES (?1, ?2)",
            params![event_type, payload],
        )?;
        Ok(self.connection.last_insert_rowid())
    }

    pub fn replay(&self) -> Result<AppState, StoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT payload FROM events ORDER BY sequence")?;
        let events = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut state = AppState::default();
        for payload in events {
            let event: DomainEvent = serde_json::from_str(&payload?)?;
            state = reduce(state, &event)?;
        }
        Ok(state)
    }
}

pub struct CasStore {
    root: PathBuf,
}

impl CasStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn put<T: Serialize>(&self, value: &T) -> Result<Hash, StoreError> {
        let bytes = serde_json::to_vec(value)?;
        self.put_bytes(&bytes)
    }

    pub fn put_bytes(&self, bytes: &[u8]) -> Result<Hash, StoreError> {
        let hash = blake3::hash(bytes);
        let path = self.path_for(hash);
        if !path.exists() {
            let temp = path.with_extension("tmp");
            std::fs::write(&temp, bytes)?;
            std::fs::rename(temp, path)?;
        }
        Ok(hash)
    }

    pub fn get<T: DeserializeOwned>(&self, hash: Hash) -> Result<T, StoreError> {
        Ok(serde_json::from_slice(&std::fs::read(
            self.path_for(hash),
        )?)?)
    }

    pub fn get_bytes(&self, hash: Hash) -> Result<Vec<u8>, StoreError> {
        Ok(std::fs::read(self.path_for(hash))?)
    }

    fn path_for(&self, hash: Hash) -> PathBuf {
        self.root.join(hash.to_hex().as_str())
    }
}

/// One sidebar workspace entry: a display name and the root directory.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceEntry {
    pub name: String,
    pub path: PathBuf,
}

/// SQLite-backed workspace list used by the TUI sidebar. Replaces the old
/// `workspaces.toml` file; see [`workspaces_from_legacy_toml`] for importing
/// the previous format.
pub struct WorkspaceStore {
    connection: Connection,
}

impl WorkspaceStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS workspaces (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                path TEXT NOT NULL UNIQUE,
                position INTEGER NOT NULL
            );",
        )?;
        Ok(Self { connection })
    }

    /// Entries in persisted (insertion) order.
    pub fn load(&self) -> Result<Vec<WorkspaceEntry>, StoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT name, path FROM workspaces ORDER BY position, id")?;
        let rows = statement.query_map([], |row| {
            Ok(WorkspaceEntry {
                name: row.get(0)?,
                path: PathBuf::from(row.get::<_, String>(1)?),
            })
        })?;
        let mut entries = Vec::new();
        for row in rows {
            entries.push(row?);
        }
        Ok(entries)
    }

    /// Replaces the whole list. Duplicate paths keep their first occurrence,
    /// matching the sidebar's dedupe-on-add behavior.
    pub fn save(&self, workspaces: &[WorkspaceEntry]) -> Result<(), StoreError> {
        let mut seen = std::collections::HashSet::new();
        let entries: Vec<&WorkspaceEntry> = workspaces
            .iter()
            .filter(|entry| seen.insert(entry.path.clone()))
            .collect();
        let transaction = self.connection.unchecked_transaction()?;
        transaction.execute("DELETE FROM workspaces", [])?;
        for (index, entry) in entries.iter().enumerate() {
            let position = i64::try_from(index).unwrap_or(i64::MAX);
            transaction.execute(
                "INSERT INTO workspaces (name, path, position) VALUES (?1, ?2, ?3)",
                params![entry.name, entry.path.to_string_lossy(), position],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum LoadedWorkspaces {
    Entries(Vec<WorkspaceEntry>),
    Paths(Vec<PathBuf>),
}

#[derive(serde::Deserialize)]
struct LoadedFile {
    workspaces: LoadedWorkspaces,
}

#[derive(serde::Deserialize)]
struct WorkspacesFile {
    workspaces: Vec<WorkspaceEntry>,
}

/// Directory name of a path, falling back to the full path text.
fn display_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.to_string_lossy().into_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// Parses a legacy `workspaces.toml`: either the current `[[workspaces]]`
/// table array with `name` + `path`, or the older flat `workspaces = [path]`
/// list whose entries get directory names. Invalid documents yield an empty
/// list so startup never fails on a stale file.
pub fn workspaces_from_legacy_toml(raw: &str) -> Vec<WorkspaceEntry> {
    toml::from_str::<WorkspacesFile>(raw)
        .map(|file| file.workspaces)
        .or_else(|_| {
            toml::from_str::<LoadedFile>(raw).map(|file| match file.workspaces {
                LoadedWorkspaces::Entries(entries) => entries,
                LoadedWorkspaces::Paths(paths) => paths
                    .into_iter()
                    .map(|path| WorkspaceEntry {
                        name: display_name(&path),
                        path,
                    })
                    .collect(),
            })
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use muxi_core::Task;
    use tempfile::tempdir;
    use time::OffsetDateTime;

    #[test]
    fn event_store_replays_events() {
        let directory = tempdir().expect("temp directory");
        let store = EventStore::open(directory.path().join("muxi.sqlite3")).expect("store");
        let task = Task::new("offline task", OffsetDateTime::UNIX_EPOCH);
        let id = task.id;
        store
            .append(&DomainEvent::TaskCreated { task })
            .expect("append");
        store
            .append(&DomainEvent::PhaseChanged {
                task_id: id,
                phase: muxi_core::Phase::Analysis,
            })
            .expect("append");
        assert_eq!(
            store.replay().expect("replay").tasks[0].phase,
            muxi_core::Phase::Analysis
        );
    }

    #[test]
    fn cas_round_trips_bytes() {
        let directory = tempdir().expect("temp directory");
        let store = CasStore::new(directory.path()).expect("cas");
        let hash = store.put_bytes(b"hello").expect("put");
        assert_eq!(store.get_bytes(hash).expect("get"), b"hello");
    }

    #[test]
    fn workspace_store_roundtrips_and_replaces() {
        let directory = tempdir().expect("temp directory");
        let store = WorkspaceStore::open(directory.path().join("muxi.sqlite3")).expect("store");
        assert!(store.load().expect("load").is_empty());
        let list = vec![
            WorkspaceEntry {
                name: "agent".to_owned(),
                path: PathBuf::from("E:\\lab\\agent"),
            },
            WorkspaceEntry {
                name: "demo".to_owned(),
                path: PathBuf::from("/tmp/demo"),
            },
        ];
        store.save(&list).expect("save");
        assert_eq!(store.load().expect("load"), list);
        let replaced = vec![WorkspaceEntry {
            name: "only".to_owned(),
            path: PathBuf::from("/tmp/only"),
        }];
        store.save(&replaced).expect("replace save");
        assert_eq!(store.load().expect("load"), replaced, "save is replace-all");
    }

    #[test]
    fn workspace_save_dedupes_by_path() {
        let directory = tempdir().expect("temp directory");
        let store = WorkspaceStore::open(directory.path().join("muxi.sqlite3")).expect("store");
        let path = PathBuf::from("E:\\lab\\agent");
        store
            .save(&[
                WorkspaceEntry {
                    name: "first".to_owned(),
                    path: path.clone(),
                },
                WorkspaceEntry {
                    name: "second".to_owned(),
                    path: path.clone(),
                },
            ])
            .expect("save");
        let loaded = store.load().expect("load");
        assert_eq!(loaded.len(), 1, "duplicate paths keep one entry");
        assert_eq!(loaded[0].name, "first");
    }

    #[test]
    fn legacy_toml_entries_parse_both_formats() {
        let current = workspaces_from_legacy_toml(
            "[[workspaces]]\nname = 'agent'\npath = 'E:\\\\lab\\\\agent'\n",
        );
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].name, "agent");
        let legacy =
            workspaces_from_legacy_toml("workspaces = ['E:\\\\lab\\\\agent', '/tmp/demo']");
        assert_eq!(
            legacy,
            vec![
                WorkspaceEntry {
                    name: "agent".to_owned(),
                    path: PathBuf::from("E:\\lab\\agent"),
                },
                WorkspaceEntry {
                    name: "demo".to_owned(),
                    path: PathBuf::from("/tmp/demo"),
                },
            ]
        );
        assert!(workspaces_from_legacy_toml("not valid toml \x01").is_empty());
        assert!(workspaces_from_legacy_toml("").is_empty());
    }
}
