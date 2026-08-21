//! Provider-neutral messages, capabilities, and deterministic scripted responses.

pub mod anthropic;

use std::collections::VecDeque;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

pub const CRATE_NAME: &str = "muxi-provider";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    pub streaming: bool,
    pub tool_use: bool,
    pub parallel_tool_use: bool,
    pub adaptive_thinking: bool,
    pub refusal_details: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentBlock {
    Text(String),
    Thinking(String),
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderEvent {
    Started,
    TextDelta(String),
    ThinkingDelta(String),
    ToolCall(ContentBlock),
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    Finished {
        stop_reason: StopReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    PauseTurn,
    Refusal,
    ModelContextWindowExceeded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRequest {
    pub model: String,
    pub prompt: String,
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("provider request was cancelled")]
    Cancelled,
    #[error("provider stream failed: {0}")]
    Failed(String),
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn capabilities(&self) -> ProviderCapabilities;

    /// Streams one turn into `events`.
    ///
    /// Contract: on `Ok(())` the provider must have sent exactly one
    /// [`ProviderEvent::Finished`] as the last event of the turn; every other
    /// outcome (transport failure, protocol violation, cancellation) returns
    /// `Err`. Callers may rely on this to release their busy state.
    async fn stream_turn(
        &self,
        request: ProviderRequest,
        events: tokio::sync::mpsc::Sender<ProviderEvent>,
        cancel: CancellationToken,
    ) -> Result<(), ProviderError>;
}

#[derive(Debug, Default)]
pub struct MockProvider {
    turns: std::sync::Mutex<VecDeque<Vec<ProviderEvent>>>,
}

impl MockProvider {
    pub fn new(turns: impl IntoIterator<Item = Vec<ProviderEvent>>) -> Self {
        Self {
            turns: std::sync::Mutex::new(turns.into_iter().collect()),
        }
    }
}

#[async_trait]
impl Provider for MockProvider {
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
        _request: ProviderRequest,
        events: tokio::sync::mpsc::Sender<ProviderEvent>,
        cancel: CancellationToken,
    ) -> Result<(), ProviderError> {
        let turn = self
            .turns
            .lock()
            .map_err(|_| ProviderError::Failed("mock provider lock poisoned".to_owned()))?
            .pop_front()
            .unwrap_or_else(|| {
                vec![ProviderEvent::Finished {
                    stop_reason: StopReason::EndTurn,
                }]
            });
        for event in turn {
            if cancel.is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
            events
                .send(event)
                .await
                .map_err(|_| ProviderError::Cancelled)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn mock_provider_replays_a_turn() {
        let provider = MockProvider::new([vec![
            ProviderEvent::Started,
            ProviderEvent::TextDelta("hello".to_owned()),
            ProviderEvent::Finished {
                stop_reason: StopReason::EndTurn,
            },
        ]]);
        let (tx, mut rx) = mpsc::channel(8);
        provider
            .stream_turn(
                ProviderRequest {
                    model: "mock".to_owned(),
                    prompt: "hi".to_owned(),
                },
                tx,
                CancellationToken::new(),
            )
            .await
            .expect("mock turn");
        assert_eq!(rx.recv().await, Some(ProviderEvent::Started));
        assert_eq!(
            rx.recv().await,
            Some(ProviderEvent::TextDelta("hello".to_owned()))
        );
    }
}
