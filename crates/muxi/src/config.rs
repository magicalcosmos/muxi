//! Muxi configuration: workspace `muxi.toml` overrides the user config file,
//! which falls back to the built-in mock provider defaults.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use muxi_provider::anthropic::AnthropicAuthKind;
use secrecy::SecretString;
use serde::Deserialize;

pub const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-5";
const DEFAULT_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
const CLAUDE_SETTINGS_FILE: &str = ".claude/settings.json";

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum ConfigSource {
    Muxi(ConfigFile),
    Claude(ClaudeSettings),
}

impl ConfigSource {
    fn into_config_file(self) -> anyhow::Result<ConfigFile> {
        match self {
            Self::Muxi(file) => Ok(file),
            Self::Claude(settings) => settings.into_config_file(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ClaudeSettings {
    env: ClaudeEnv,
}

#[derive(Debug, Clone, Deserialize)]
struct ClaudeEnv {
    #[serde(rename = "ANTHROPIC_AUTH_TOKEN")]
    auth_token: String,
    #[serde(rename = "ANTHROPIC_BASE_URL")]
    base_url: String,
    #[serde(rename = "ANTHROPIC_DEFAULT_SONNET_MODEL")]
    sonnet_model: Option<String>,
    #[serde(rename = "ANTHROPIC_MODEL")]
    model: Option<String>,
}

impl ClaudeSettings {
    fn into_config_file(self) -> anyhow::Result<ConfigFile> {
        if self.env.auth_token.trim().is_empty() {
            bail!("Claude Code settings contain an empty ANTHROPIC_AUTH_TOKEN");
        }
        if self.env.base_url.trim().is_empty() {
            bail!("Claude Code settings contain an empty ANTHROPIC_BASE_URL");
        }
        Ok(ConfigFile {
            provider: ProviderSection {
                kind: ProviderKind::Anthropic,
                model: self.env.model.or(self.env.sonnet_model),
                base_url: Some(self.env.base_url),
                api_key_env: None,
                inline_api_key: Some(self.env.auth_token),
                auth_kind: AnthropicAuthKind::Bearer,
            },
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    #[serde(default)]
    pub provider: ProviderSection,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderSection {
    /// `mock` (default) or `anthropic`.
    #[serde(default)]
    pub kind: ProviderKind,
    pub model: Option<String>,
    pub base_url: Option<String>,
    /// Environment variable holding the API key. Defaults to `ANTHROPIC_API_KEY`.
    pub api_key_env: Option<String>,
    /// Inline API key/token, used only when importing Claude Code settings.
    #[serde(skip)]
    pub inline_api_key: Option<String>,
    /// HTTP auth style, used only when importing Claude Code settings.
    #[serde(skip)]
    pub auth_kind: AnthropicAuthKind,
}

impl Default for ProviderSection {
    fn default() -> Self {
        Self {
            kind: ProviderKind::Mock,
            model: None,
            base_url: None,
            api_key_env: None,
            inline_api_key: None,
            auth_kind: AnthropicAuthKind::ApiKey,
        }
    }
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
        auth_kind: AnthropicAuthKind,
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
    let source = candidates
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

    let file = source.map(ConfigSource::into_config_file).transpose()?;
    resolve(file.as_ref())
}

fn config_candidates(workspace: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![workspace.join("muxi.toml")];
    if let Some(base) = directories::BaseDirs::new() {
        candidates.push(base.home_dir().join(CLAUDE_SETTINGS_FILE));
        candidates.push(base.home_dir().join(".muxi").join("config.toml"));
        candidates.push(base.config_dir().join(".muxi").join("config.toml"));
    }
    candidates
}

fn read_config(path: &Path) -> anyhow::Result<Option<ConfigSource>> {
    if !path.is_file() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read config file {}", path.display()))?;
    let source = if path.ends_with(CLAUDE_SETTINGS_FILE) {
        let parsed = serde_json::from_str::<ClaudeSettings>(&raw)
            .with_context(|| format!("cannot parse config file {}", path.display()))?;
        ConfigSource::Claude(parsed)
    } else {
        let parsed = toml::from_str::<ConfigFile>(&raw)
            .with_context(|| format!("cannot parse config file {}", path.display()))?;
        ConfigSource::Muxi(parsed)
    };
    Ok(Some(source))
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
            let api_key = file
                .provider
                .inline_api_key
                .clone()
                .or_else(|| {
                    let env_var = file
                        .provider
                        .api_key_env
                        .clone()
                        .unwrap_or_else(|| DEFAULT_API_KEY_ENV.to_owned());
                    env(&env_var).filter(|key| !key.is_empty())
                })
                .ok_or_else(|| {
                    let env_var = file
                        .provider
                        .api_key_env
                        .clone()
                        .unwrap_or_else(|| DEFAULT_API_KEY_ENV.to_owned());
                    MissingApiKey { env_var }
                })?;
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
                auth_kind: file.provider.auth_kind,
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
                ..ProviderSection::default()
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
                ..ProviderSection::default()
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
                ..ProviderSection::default()
            },
        };
        let resolved =
            resolve_with_env(Some(&file), |_| Some("test-key".to_owned())).expect("resolve");
        let ResolvedProvider::Anthropic {
            model,
            base_url,
            api_key: _,
            auth_kind: _,
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
                ..ProviderSection::default()
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
    fn claude_settings_are_first_global_config() {
        let candidates = config_candidates(Path::new("/ws"));
        assert_eq!(candidates[0], Path::new("/ws").join("muxi.toml"));
        let claude_settings = candidates
            .get(1)
            .expect("Claude Code settings candidate requires a home directory");
        assert!(claude_settings.ends_with(CLAUDE_SETTINGS_FILE));
    }

    #[test]
    fn home_dot_muxi_is_manual_fallback() {
        let candidates = config_candidates(Path::new("/ws"));
        let home_global = candidates
            .get(2)
            .expect("home config candidate requires a home directory");
        assert!(home_global.ends_with(".muxi/config.toml"));
    }

    #[test]
    fn appdata_dot_muxi_is_legacy_fallback() {
        let candidates = config_candidates(Path::new("/ws"));
        let appdata_global = candidates
            .get(3)
            .expect("app data config candidate requires a config directory");
        assert!(appdata_global.ends_with(".muxi/config.toml"));
    }

    #[test]
    fn parses_claude_code_settings() {
        let raw = r#"
{
  "env": {
    "ANTHROPIC_AUTH_TOKEN": "PROXY_MANAGED",
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:5000",
    "ANTHROPIC_DEFAULT_SONNET_MODEL": "claude-sonnet-4-6"
  }
}
"#;
        let parsed: ClaudeSettings = serde_json::from_str(raw).expect("parse settings");
        let file = parsed.into_config_file().expect("convert settings");
        assert_eq!(file.provider.kind, ProviderKind::Anthropic);
        assert_eq!(file.provider.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(
            file.provider.base_url.as_deref(),
            Some("http://127.0.0.1:5000")
        );
        assert_eq!(
            file.provider.inline_api_key.as_deref(),
            Some("PROXY_MANAGED")
        );
        assert_eq!(file.provider.auth_kind, AnthropicAuthKind::Bearer);
    }

    #[test]
    fn rejects_unknown_fields() {
        let raw = "[provider]\nkind = \"mock\"\nunknown = 1\n";
        assert!(toml::from_str::<ConfigFile>(raw).is_err());
    }
}
