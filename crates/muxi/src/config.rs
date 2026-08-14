//! Muxi configuration: workspace `muxi.toml` overrides the user config file,
//! which falls back to the built-in mock provider defaults.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use secrecy::SecretString;
use serde::Deserialize;

pub const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-5";
const DEFAULT_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    #[serde(default)]
    pub provider: ProviderSection,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ProviderSection {
    /// `mock` (default) or `anthropic`.
    #[serde(default)]
    pub kind: ProviderKind,
    pub model: Option<String>,
    pub base_url: Option<String>,
    /// Environment variable holding the API key. Defaults to `ANTHROPIC_API_KEY`.
    pub api_key_env: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    #[default]
    Mock,
    Anthropic,
}

/// Fully resolved runtime configuration. The API key is compared as opaque
/// (`PartialEq` is not implemented for secrets).
#[derive(Debug, Clone)]
pub enum ResolvedProvider {
    Mock,
    Anthropic {
        model: String,
        base_url: String,
        api_key: SecretString,
    },
}

#[derive(Debug, thiserror::Error)]
#[error("provider `anthropic` is configured but the {env_var} environment variable is not set")]
pub struct MissingApiKey {
    pub env_var: String,
}

/// Loads configuration: workspace `muxi.toml` first, then the user config file.
///
/// # Errors
///
/// Returns an error when a found config file cannot be read or parsed, or when
/// an anthropic provider is configured without a resolvable API key.
pub fn load(workspace: &Path) -> anyhow::Result<ResolvedProvider> {
    let candidates = config_candidates(workspace);
    let file = candidates
        .iter()
        .find_map(|path| read_config(path).transpose())
        .transpose()
        .map_err(|error| {
            let path = candidates
                .iter()
                .find(|path| path.is_file())
                .cloned()
                .unwrap_or_else(|| PathBuf::from("muxi.toml"));
            error.context(format!("invalid config in {}", path.display()))
        })?;

    resolve(file.as_ref())
}

fn config_candidates(workspace: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![workspace.join("muxi.toml")];
    if let Some(base) = directories::BaseDirs::new() {
        candidates.push(base.config_dir().join(".muxi").join("config.toml"));
    }
    candidates
}

fn read_config(path: &Path) -> anyhow::Result<Option<ConfigFile>> {
    if !path.is_file() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read config file {}", path.display()))?;
    let parsed = toml::from_str::<ConfigFile>(&raw)
        .with_context(|| format!("cannot parse config file {}", path.display()))?;
    Ok(Some(parsed))
}

fn resolve(file: Option<&ConfigFile>) -> anyhow::Result<ResolvedProvider> {
    resolve_with_env(file, |name| std::env::var(name).ok())
}

fn resolve_with_env(
    file: Option<&ConfigFile>,
    env: impl Fn(&str) -> Option<String>,
) -> anyhow::Result<ResolvedProvider> {
    let Some(file) = file else {
        return Ok(ResolvedProvider::Mock);
    };
    match file.provider.kind {
        ProviderKind::Mock => Ok(ResolvedProvider::Mock),
        ProviderKind::Anthropic => {
            let env_var = file
                .provider
                .api_key_env
                .clone()
                .unwrap_or_else(|| DEFAULT_API_KEY_ENV.to_owned());
            let Some(api_key) = env(&env_var).filter(|key| !key.is_empty()) else {
                return Err(MissingApiKey { env_var }.into());
            };
            let model = file
                .provider
                .model
                .clone()
                .unwrap_or_else(|| DEFAULT_ANTHROPIC_MODEL.to_owned());
            if model.trim().is_empty() {
                bail!("provider `anthropic` has an empty model");
            }
            let base_url = file.provider.base_url.clone().unwrap_or_else(|| {
                muxi_provider::anthropic::AnthropicConfig::default_base_url().to_owned()
            });
            Ok(ResolvedProvider::Anthropic {
                model,
                base_url,
                api_key: SecretString::from(api_key),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_config_defaults_to_mock() {
        let resolved = resolve_with_env(None, |_| None).expect("resolve");
        assert!(matches!(resolved, ResolvedProvider::Mock));
    }

    #[test]
    fn mock_kind_resolves_without_key() {
        let file = ConfigFile::default();
        let resolved = resolve_with_env(Some(&file), |_| None).expect("resolve");
        assert!(matches!(resolved, ResolvedProvider::Mock));
    }

    #[test]
    fn anthropic_requires_env_key() {
        let file = ConfigFile {
            provider: ProviderSection {
                kind: ProviderKind::Anthropic,
                model: Some("claude-sonnet-5".to_owned()),
                base_url: None,
                api_key_env: Some("MUXI_TEST_ABSENT_KEY".to_owned()),
            },
        };
        let error = resolve_with_env(Some(&file), |_| None).expect_err("must require key");
        assert!(error.to_string().contains("MUXI_TEST_ABSENT_KEY"));
    }

    #[test]
    fn anthropic_rejects_empty_key() {
        let file = ConfigFile {
            provider: ProviderSection {
                kind: ProviderKind::Anthropic,
                model: None,
                base_url: None,
                api_key_env: Some("MUXI_TEST_EMPTY_KEY".to_owned()),
            },
        };
        let error = resolve_with_env(Some(&file), |_| Some(String::new()))
            .expect_err("must reject empty key");
        assert!(error.to_string().contains("MUXI_TEST_EMPTY_KEY"));
    }

    #[test]
    fn anthropic_resolves_with_defaults() {
        let file = ConfigFile {
            provider: ProviderSection {
                kind: ProviderKind::Anthropic,
                model: None,
                base_url: None,
                api_key_env: Some("MUXI_TEST_PRESENT_KEY".to_owned()),
            },
        };
        let resolved =
            resolve_with_env(Some(&file), |_| Some("test-key".to_owned())).expect("resolve");
        let ResolvedProvider::Anthropic {
            model,
            base_url,
            api_key: _,
        } = &resolved
        else {
            panic!("expected anthropic provider, got {resolved:?}");
        };
        assert_eq!(model, DEFAULT_ANTHROPIC_MODEL);
        assert_eq!(base_url, "https://api.anthropic.com");
    }

    #[test]
    fn anthropic_resolves_custom_base_url() {
        let file = ConfigFile {
            provider: ProviderSection {
                kind: ProviderKind::Anthropic,
                model: Some("claude-opus-5".to_owned()),
                base_url: Some("http://localhost:8080".to_owned()),
                api_key_env: None,
            },
        };
        let resolved =
            resolve_with_env(Some(&file), |_| Some("test-key".to_owned())).expect("resolve");
        let ResolvedProvider::Anthropic { base_url, .. } = &resolved else {
            panic!("expected anthropic provider, got {resolved:?}");
        };
        assert_eq!(base_url, "http://localhost:8080");
    }

    #[test]
    fn parses_workspace_toml() {
        let raw = r#"
[provider]
kind = "anthropic"
model = "claude-opus-5"
"#;
        let parsed: ConfigFile = toml::from_str(raw).expect("parse");
        assert_eq!(parsed.provider.kind, ProviderKind::Anthropic);
        assert_eq!(parsed.provider.model.as_deref(), Some("claude-opus-5"));
    }

    #[test]
    fn global_config_lives_in_dot_muxi_dir() {
        let candidates = config_candidates(Path::new("/ws"));
        assert_eq!(candidates[0], Path::new("/ws").join("muxi.toml"));
        let global = candidates
            .get(1)
            .expect("global config candidate requires a home directory");
        assert!(global.ends_with(".muxi/config.toml"));
    }

    #[test]
    fn rejects_unknown_fields() {
        let raw = "[provider]\nkind = \"mock\"\nunknown = 1\n";
        assert!(toml::from_str::<ConfigFile>(raw).is_err());
    }
}
