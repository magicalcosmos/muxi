//! Muxi executable: argument parsing, configuration, and runtime composition.

mod config;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use config::{ResolvedProvider, load};
use muxi_provider::MockProvider;
use muxi_provider::anthropic::{AnthropicConfig, AnthropicProvider};
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
    let workspace = std::fs::canonicalize(&cli.workspace).map_err(|error| {
        anyhow::anyhow!("cannot open workspace {}: {error}", cli.workspace.display())
    })?;

    let resolved = load(&workspace).context("cannot load muxi configuration")?;
    let context = build_context(resolved);
    muxi_tui::run(&workspace, context)?;
    Ok(())
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
        } => TuiContext {
            provider: Arc::new(AnthropicProvider::new(
                AnthropicConfig::new(api_key, model.clone()).with_base_url(base_url),
            )),
            provider_label: "anthropic".to_owned(),
            model,
        },
    }
}
