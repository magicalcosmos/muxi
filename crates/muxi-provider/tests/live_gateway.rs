//! Live end-to-end check against the real Anthropic-compatible gateway.
//!
//! Ignored by default so tests stay free of real network calls and token
//! spend (see README). Run it explicitly with:
//!
//! ```sh
//! cargo test -p muxi-provider --test live_gateway -- --ignored --nocapture
//! ```
//!
//! Reads the same Claude Code `settings.json` the main program uses, so it
//! exercises exactly the configuration path Muxi takes at runtime.

use std::path::Path;

use muxi_provider::anthropic::{AnthropicAuthKind, AnthropicConfig, AnthropicProvider};
use muxi_provider::{Provider, ProviderEvent, ProviderRequest};
use secrecy::SecretString;

#[tokio::test]
#[ignore = "spends real tokens and requires the gateway from Claude Code settings"]
async fn live_gateway_turn_completes() {
    let home = std::env::var("USERPROFILE").expect("USERPROFILE must be set");
    let settings = std::fs::read_to_string(Path::new(&home).join(".claude/settings.json"))
        .expect("read Claude Code settings.json");
    let value: serde_json::Value = serde_json::from_str(&settings).expect("parse settings.json");
    let env = value.get("env").expect("settings.json has no env section");
    let field = |name: &str| {
        env.get(name)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| panic!("settings.json env.{name} is missing or empty"))
    };

    let config = AnthropicConfig::new(
        SecretString::from(field("ANTHROPIC_AUTH_TOKEN").to_owned()),
        field("ANTHROPIC_MODEL"),
    )
    .with_base_url(field("ANTHROPIC_BASE_URL"))
    .with_auth_kind(AnthropicAuthKind::Bearer);
    let provider = AnthropicProvider::new(config);

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    provider
        .stream_turn(
            ProviderRequest {
                model: String::new(),
                prompt: "用一句话回答：1+1等于几？".to_owned(),
            },
            tx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("live gateway turn must succeed");

    let mut text = String::new();
    let mut thinking = String::new();
    let mut finished = false;
    while let Some(event) = rx.recv().await {
        match event {
            ProviderEvent::TextDelta(delta) => text.push_str(&delta),
            ProviderEvent::ThinkingDelta(delta) => thinking.push_str(&delta),
            ProviderEvent::Finished { stop_reason } => {
                finished = true;
                println!("finished with {stop_reason:?}");
            }
            _ => {}
        }
    }
    assert!(finished, "turn must reach Finished");
    assert!(
        !text.trim().is_empty(),
        "expected non-empty reply text, got thinking={thinking:?} text={text:?}"
    );
    println!("thinking: {thinking}");
    println!("text: {text}");
}
