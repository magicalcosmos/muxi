//! Anthropic Messages API adapter with SSE streaming.

use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::{
    Provider, ProviderCapabilities, ProviderError, ProviderEvent, ProviderRequest, StopReason,
};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u64 = 4096;

#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    pub api_key: SecretString,
    pub model: String,
    pub base_url: String,
    pub max_tokens: u64,
}

impl AnthropicConfig {
    #[must_use]
    pub fn default_base_url() -> &'static str {
        DEFAULT_BASE_URL
    }

    #[must_use]
    pub fn new(api_key: SecretString, model: impl Into<String>) -> Self {
        Self {
            api_key,
            model: model.into(),
            base_url: DEFAULT_BASE_URL.to_owned(),
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    }

    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }
}

#[derive(Debug)]
pub struct AnthropicProvider {
    config: AnthropicConfig,
    client: reqwest::Client,
}

impl AnthropicProvider {
    pub fn new(config: AnthropicConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .unwrap_or_default();
        Self { config, client }
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_use: true,
            parallel_tool_use: true,
            adaptive_thinking: true,
            refusal_details: true,
        }
    }

    async fn stream_turn(
        &self,
        request: ProviderRequest,
        events: tokio::sync::mpsc::Sender<ProviderEvent>,
        cancel: CancellationToken,
    ) -> Result<(), ProviderError> {
        let model = if request.model.is_empty() {
            self.config.model.clone()
        } else {
            request.model
        };
        let body = serde_json::json!({
            "model": model,
            "max_tokens": self.config.max_tokens,
            "stream": true,
            "messages": [
                { "role": "user", "content": request.prompt }
            ]
        });

        let response = self
            .client
            .post(format!("{}/v1/messages", self.config.base_url))
            .header("x-api-key", self.config.api_key.expose_secret())
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body)
            .send()
            .await
            .map_err(|error| ProviderError::Failed(format!("request failed: {error}")))?;

        let status = response.status();
        if !status.is_success() {
            let detail = response.text().await.unwrap_or_else(|_| status.to_string());
            return Err(ProviderError::Failed(format!(
                "anthropic returned {status}: {detail}"
            )));
        }

        events
            .send(ProviderEvent::Started)
            .await
            .map_err(|_| ProviderError::Cancelled)?;

        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        loop {
            if cancel.is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
            let chunk = tokio::select! {
                chunk = stream.next() => chunk,
                () = cancel.cancelled() => return Err(ProviderError::Cancelled),
            };
            let Some(chunk) = chunk else { break };
            let chunk = chunk
                .map_err(|error| ProviderError::Failed(format!("stream read failed: {error}")))?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(position) = buffer.find("\n\n") {
                let block: String = buffer.drain(..position + 2).collect();
                for event in parse_sse_block(&block)? {
                    events
                        .send(event)
                        .await
                        .map_err(|_| ProviderError::Cancelled)?;
                }
            }
        }
        Ok(())
    }
}

/// Parses one SSE block (`event: ...\ndata: {...}\n\n`) into provider events.
///
/// Returns `Err` for `error` stream events so the turn surfaces the failure.
fn parse_sse_block(block: &str) -> Result<Vec<ProviderEvent>, ProviderError> {
    let mut events = Vec::new();
    for line in block.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        events.extend(parse_data(data)?);
    }
    Ok(events)
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum StreamMessage {
    #[serde(rename = "message_start")]
    MessageStart { message: MessageBody },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta { delta: Delta },
    #[serde(rename = "message_delta")]
    MessageDelta {
        delta: StopDelta,
        usage: OutputUsage,
    },
    #[serde(rename = "message_stop")]
    MessageStop,
    #[serde(rename = "error")]
    Error { error: ApiError },
}

#[derive(Debug, Deserialize)]
struct MessageBody {
    #[serde(default)]
    usage: InputUsage,
}

#[derive(Debug, Deserialize, Default)]
struct InputUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

#[derive(Debug, Deserialize, Default)]
struct OutputUsage {
    #[serde(default)]
    output_tokens: u64,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Delta {
    TextDelta { text: String },
}

#[derive(Debug, Deserialize)]
struct StopDelta {
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    #[serde(default)]
    message: String,
}

/// Parses one SSE `data:` payload. Unknown event kinds are skipped.
fn parse_data(data: &str) -> Result<Vec<ProviderEvent>, ProviderError> {
    // Unknown event kinds fail to deserialize and are skipped.
    let Ok(message) = serde_json::from_str::<StreamMessage>(data) else {
        return Ok(Vec::new());
    };
    match message {
        StreamMessage::MessageStart { message } => Ok(vec![ProviderEvent::Usage {
            input_tokens: message.usage.input_tokens,
            output_tokens: message.usage.output_tokens,
        }]),
        StreamMessage::ContentBlockDelta { delta } => match delta {
            Delta::TextDelta { text } => Ok(vec![ProviderEvent::TextDelta(text)]),
        },
        StreamMessage::MessageDelta { delta, usage } => Ok(vec![
            ProviderEvent::Usage {
                input_tokens: 0,
                output_tokens: usage.output_tokens,
            },
            ProviderEvent::Finished {
                stop_reason: map_stop_reason(delta.stop_reason.as_deref()),
            },
        ]),
        StreamMessage::MessageStop => Ok(Vec::new()),
        StreamMessage::Error { error } => Err(ProviderError::Failed(format!(
            "anthropic stream error: {}",
            error.message
        ))),
    }
}

fn map_stop_reason(reason: Option<&str>) -> StopReason {
    match reason {
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        Some("pause_turn") => StopReason::PauseTurn,
        Some("refusal") => StopReason::Refusal,
        _ => StopReason::EndTurn,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_delta() {
        let events = parse_sse_block(
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n",
        )
        .expect("parse");
        assert_eq!(events, vec![ProviderEvent::TextDelta("hello".to_owned())]);
    }

    #[test]
    fn parses_message_start_usage() {
        let events = parse_sse_block(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":12,\"output_tokens\":1}}}\n\n",
        )
        .expect("parse");
        assert_eq!(
            events,
            vec![ProviderEvent::Usage {
                input_tokens: 12,
                output_tokens: 1
            }]
        );
    }

    #[test]
    fn parses_message_delta_stop_reason() {
        let events = parse_sse_block(
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":34}}\n\n",
        )
        .expect("parse");
        assert_eq!(
            events,
            vec![
                ProviderEvent::Usage {
                    input_tokens: 0,
                    output_tokens: 34
                },
                ProviderEvent::Finished {
                    stop_reason: StopReason::EndTurn
                }
            ]
        );
    }

    #[test]
    fn ignores_ping_and_message_stop() {
        let events = parse_sse_block(
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\nevent: ping\ndata: {\"type\":\"ping\"}\n\n",
        )
        .expect("parse");
        assert!(events.is_empty());
    }

    #[test]
    fn surfaces_stream_errors() {
        let result = parse_sse_block(
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
        );
        assert!(result.is_err());
    }
}
