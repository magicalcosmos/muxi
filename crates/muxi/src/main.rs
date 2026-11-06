//! Muxi executable: argument parsing, configuration, and runtime composition.

mod config;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use config::{ResolvedProvider, load};
use muxi_provider::MockProvider;
use muxi_provider::anthropic::{AnthropicConfig, AnthropicProvider};
use muxi_tools::Workspace;
use muxi_tui::TuiContext;

#[derive(Debug, Parser)]
#[command(
    name = "muxi",
    version,
    about = "A task-centered Vim-modal coding agent"
)]
struct Cli {
    /// Workspace to open. Defaults to the current directory.
    #[arg(default_value = ".")]
    workspace: PathBuf,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let workspace = Workspace::new(&cli.workspace).map_err(|error| {
        anyhow::anyhow!("cannot open workspace {}: {error}", cli.workspace.display())
    })?;

    let resolved = load(workspace.root()).context("cannot load muxi configuration")?;
    let context = build_context(resolved);

    let store_path = state_db_path();
    let store =
        muxi_store::WorkspaceStore::open(&store_path).context("cannot open state database")?;
    let mut workspaces = store.load().context("cannot load workspaces")?;
    if workspaces.is_empty() {
        workspaces = import_legacy_workspaces(&store);
    }
    let root = workspace.root().to_path_buf();
    let active = workspaces
        .iter()
        .position(|entry| entry.path == root)
        .unwrap_or_else(|| {
            let name = root.file_name().map_or_else(
                || root.to_string_lossy().into_owned(),
                |name| name.to_string_lossy().into_owned(),
            );
            workspaces.push(muxi_store::WorkspaceEntry { name, path: root });
            store.save(&workspaces).ok();
            workspaces.len() - 1
        });

    muxi_tui::run(workspace, workspaces, active, Some(store_path), context)?;
    Ok(())
}

/// `SQLite` database for persistent state, next to the muxi global config.
fn state_db_path() -> PathBuf {
    directories::BaseDirs::new().map_or_else(
        || PathBuf::from("muxi.db"),
        |base| base.home_dir().join(".muxi").join("muxi.db"),
    )
}

/// Pre-SQLite installations kept the sidebar list in `workspaces.toml`;
/// import it once into the database and archive the file.
fn import_legacy_workspaces(store: &muxi_store::WorkspaceStore) -> Vec<muxi_store::WorkspaceEntry> {
    let Some(legacy) = directories::BaseDirs::new()
        .map(|base| base.home_dir().join(".muxi").join("workspaces.toml"))
    else {
        return Vec::new();
    };
    let Ok(raw) = std::fs::read_to_string(&legacy) else {
        return Vec::new();
    };
    let entries = muxi_store::workspaces_from_legacy_toml(&raw);
    if !entries.is_empty() {
        store.save(&entries).ok();
    }
    let archived = legacy.with_extension("toml.imported");
    let _ = std::fs::rename(&legacy, archived);
    entries
}

fn build_context(resolved: ResolvedProvider) -> TuiContext {
    match resolved {
        ResolvedProvider::Mock => TuiContext {
            provider: Arc::new(MockProvider::default()),
            provider_label: "mock".to_owned(),
            model: "-".to_owned(),
        },
        ResolvedProvider::Anthropic {
            model,
            base_url,
            api_key,
            auth_kind,
        } => TuiContext {
            provider: Arc::new(AnthropicProvider::new(
                AnthropicConfig::new(api_key, model.clone())
                    .with_base_url(base_url)
                    .with_auth_kind(auth_kind),
            )),
            provider_label: "anthropic".to_owned(),
            model,
        },
    }
}
