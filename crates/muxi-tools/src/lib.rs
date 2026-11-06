//! Workspace-safe file primitives used by the first tool slice.

use std::path::{Path, PathBuf};

use blake3::Hash;
use thiserror::Error;

pub const CRATE_NAME: &str = "muxi-tools";

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("path escapes workspace: {0}")]
    PathEscapesWorkspace(PathBuf),
    #[error("file is not valid UTF-8: {0}")]
    InvalidUtf8(PathBuf),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSnapshot {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
    pub hash: Hash,
}

/// Canonicalizes a path and strips the Windows extended-length prefix
/// (`\\?\C:\...`), which `std::fs::canonicalize` keeps even for ordinary
/// paths. The stripped form displays naturally and compares equal to
/// user-entered paths.
pub fn canonicalize(path: impl AsRef<Path>) -> std::io::Result<PathBuf> {
    let canonical = std::fs::canonicalize(path)?;
    let text = canonical.to_string_lossy();
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        Ok(PathBuf::from(format!(r"\\{unc}")))
    } else if let Some(plain) = text.strip_prefix(r"\\?\") {
        Ok(PathBuf::from(plain))
    } else {
        Ok(canonical)
    }
}

#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    pub fn new(root: impl AsRef<Path>) -> Result<Self, ToolError> {
        let root = canonicalize(root)?;
        if !root.is_dir() {
            return Err(ToolError::PathEscapesWorkspace(root));
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn resolve(&self, path: impl AsRef<Path>) -> Result<PathBuf, ToolError> {
        let path = path.as_ref();
        if path.is_absolute() {
            return Err(ToolError::PathEscapesWorkspace(path.to_path_buf()));
        }
        let candidate = self.root.join(path);
        let existing = if candidate.exists() {
            canonicalize(&candidate)?
        } else {
            let parent = candidate
                .parent()
                .ok_or_else(|| ToolError::PathEscapesWorkspace(candidate.clone()))?;
            canonicalize(parent)?.join(
                candidate
                    .file_name()
                    .ok_or_else(|| ToolError::PathEscapesWorkspace(candidate.clone()))?,
            )
        };
        if existing == self.root || existing.starts_with(&self.root) {
            Ok(existing)
        } else {
            Err(ToolError::PathEscapesWorkspace(existing))
        }
    }

    pub fn read(&self, path: impl AsRef<Path>) -> Result<FileSnapshot, ToolError> {
        let resolved = self.resolve(path)?;
        let bytes = std::fs::read(&resolved)?;
        let hash = blake3::hash(&bytes);
        Ok(FileSnapshot {
            path: resolved,
            bytes,
            hash,
        })
    }

    pub fn read_text(&self, path: impl AsRef<Path>) -> Result<(FileSnapshot, String), ToolError> {
        let snapshot = self.read(path)?;
        let text = String::from_utf8(snapshot.bytes.clone())
            .map_err(|_| ToolError::InvalidUtf8(snapshot.path.clone()))?;
        Ok((snapshot, text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn reads_with_a_stable_hash() {
        let dir = tempdir().expect("temp directory");
        std::fs::write(dir.path().join("main.rs"), b"fn main() {}\n").expect("write");
        let workspace = Workspace::new(dir.path()).expect("workspace");
        let snapshot = workspace.read("main.rs").expect("read");
        assert_eq!(snapshot.hash, blake3::hash(b"fn main() {}\n"));
    }

    #[test]
    fn rejects_parent_escape() {
        let dir = tempdir().expect("temp directory");
        let workspace = Workspace::new(dir.path()).expect("workspace");
        assert!(matches!(
            workspace.resolve(".."),
            Err(ToolError::PathEscapesWorkspace(_))
        ));
    }

    #[test]
    fn canonicalize_strips_windows_extended_prefix() {
        let dir = tempdir().expect("temp directory");
        let stripped = canonicalize(dir.path()).expect("canonicalize");
        let text = stripped.to_string_lossy();
        assert!(
            !text.starts_with(r"\\?\"),
            "extended-length prefix must be stripped, got: {text}"
        );
        assert_eq!(
            Workspace::new(dir.path()).expect("workspace").root(),
            stripped
        );
    }
}
