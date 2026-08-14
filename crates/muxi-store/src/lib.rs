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
}
