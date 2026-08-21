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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AnthropicAuthKind {
    #[default]
    ApiKey,
    Bearer,
}

#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    pub api_key: SecretString,
    pub auth_kind: AnthropicAuthKind,
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
            auth_kind: AnthropicAuthKind::ApiKey,
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

    #[must_use]
    pub fn with_auth_kind(mut self, auth_kind: AnthropicAuthKind) -> Self {
        self.auth_kind = auth_kind;
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

    fn messages_url(&self) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        format!("{base}/v1/messages")
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
            refusal_details: false,
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

        let request = self.client.post(self.messages_url());
        let request = match self.config.auth_kind {
            AnthropicAuthKind::ApiKey => {
                request.header("x-api-key", self.config.api_key.expose_secret())
            }
            AnthropicAuthKind::Bearer => request.bearer_auth(self.config.api_key.expose_secret()),
        };
        let response = request
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

        let mut decoder = SseDecoder::new();
        let mut stream = response.bytes_stream();
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
            for event in decoder.push(chunk.as_ref())? {
                events
                    .send(event)
                    .await
                    .map_err(|_| ProviderError::Cancelled)?;
            }
        }
        if let Some(terminal) = decoder.finish()? {
            events
                .send(terminal)
                .await
                .map_err(|_| ProviderError::Cancelled)?;
        }
        Ok(())
    }
}

/// Incremental byte-level SSE decoder for one Anthropic streaming turn.
///
/// Frames events on complete lines (LF, CRLF, or lone CR all accepted) and
/// dispatches each complete `data:` payload immediately, so event boundaries
/// split across network chunks just work. `message_delta` only records the
/// stop reason; `message_stop` returns the single terminal `Finished` event.
/// `finish` enforces that the stream actually reached `message_stop`.
struct SseDecoder {
    /// Raw bytes of the line currently being assembled.
    line: Vec<u8>,
    /// Data payload lines accumulated for the event being assembled.
    data_lines: Vec<Vec<u8>>,
    /// True once the previous byte was a CR that may pair with an LF.
    pending_cr: bool,
    stop_reason: Option<StopReason>,
    saw_message_stop: bool,
    finished_sent: bool,
}

impl SseDecoder {
    fn new() -> Self {
        Self {
            line: Vec::new(),
            data_lines: Vec::new(),
            pending_cr: false,
            stop_reason: None,
            saw_message_stop: false,
            finished_sent: false,
        }
    }

    /// Feeds one network chunk and returns the events completed by it.
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<ProviderEvent>, ProviderError> {
        let mut events = Vec::new();
        for &byte in chunk {
            match byte {
                b'\n' => {
                    // A pending CR makes this the second half of a CRLF pair;
                    // either way the line is now complete.
                    self.pending_cr = false;
                    events.extend(self.end_line()?);
                }
                b'\r' => {
                    // A lone CR also terminates a line per SSE; wait one byte
                    // so CRLF is treated as a single terminator.
                    if self.pending_cr {
                        events.extend(self.end_line()?);
                    }
                    self.pending_cr = true;
                }
                _ => {
                    if self.pending_cr {
                        self.pending_cr = false;
                        events.extend(self.end_line()?);
                    }
                    self.line.push(byte);
                }
            }
        }
        Ok(events)
    }

    /// Handles end-of-stream: flushes a trailing unterminated line and then
    /// enforces the terminal-state contract.
    fn finish(mut self) -> Result<Option<ProviderEvent>, ProviderError> {
        if !self.line.is_empty() || self.pending_cr {
            let pending = std::mem::take(&mut self.line);
            self.data_lines.push(pending);
            let payload = self.take_data_lines();
            self.handle_event(&payload)?;
        } else if !self.data_lines.is_empty() {
            // Trailing `data:` lines without a final blank line.
            let payload = self.take_data_lines();
            self.handle_event(&payload)?;
        }
        if !self.saw_message_stop {
            return Err(ProviderError::Failed(
                "stream ended before message_stop".to_owned(),
            ));
        }
        Ok(if self.finished_sent {
            None
        } else {
            self.finished_sent = true;
            let stop_reason = self.stop_reason.ok_or_else(|| {
                ProviderError::Failed("message_stop arrived without a stop_reason".to_owned())
            })?;
            Some(ProviderEvent::Finished { stop_reason })
        })
    }

    /// Completes the current line, dispatching it as SSE field, separator, or
    /// neither.
    fn end_line(&mut self) -> Result<Vec<ProviderEvent>, ProviderError> {
        let line = std::mem::take(&mut self.line);
        if line.is_empty() {
            // Blank line: dispatch the accumulated event, if any.
            if self.data_lines.is_empty() {
                return Ok(Vec::new());
            }
            let payload = self.take_data_lines();
            let mut events = self.handle_event(&payload)?;
            if let Some(terminal) = self.take_pending_terminal()? {
                events.push(terminal);
            }
            return Ok(events);
        }
        if line.first() == Some(&b':') {
            // Comment / heartbeat line.
            return Ok(Vec::new());
        }
        if let Some(value) = strip_field(&line, b"data") {
            self.data_lines.push(value.to_vec());
        }
        // `event:`, `id:`, `retry:` and unknown fields are ignored.
        Ok(Vec::new())
    }

    fn take_data_lines(&mut self) -> Vec<u8> {
        let lines = std::mem::take(&mut self.data_lines);
        let mut payload = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            if index > 0 {
                payload.push(b'\n');
            }
            payload.extend_from_slice(line);
        }
        payload
    }

    /// Parses one complete `data:` payload into provider events.
    fn handle_event(&mut self, payload: &[u8]) -> Result<Vec<ProviderEvent>, ProviderError> {
        if payload.is_empty() {
            return Ok(Vec::new());
        }
        let text = std::str::from_utf8(payload).map_err(|error| {
            ProviderError::Failed(format!("stream contained invalid UTF-8: {error}"))
        })?;
        let kind = event_kind(text)?;
        match kind {
            Some(EventKind::Known) => {
                let message = serde_json::from_str::<StreamMessage>(text).map_err(|error| {
                    ProviderError::Failed(format!("malformed stream event: {error}"))
                })?;
                match message {
                    StreamMessage::MessageStart { message } => Ok(vec![ProviderEvent::Usage {
                        input_tokens: message.usage.input_tokens,
                        output_tokens: message.usage.output_tokens,
                    }]),
                    StreamMessage::ContentBlockDelta { delta } => match delta {
                        Delta::TextDelta { text } => Ok(vec![ProviderEvent::TextDelta(text)]),
                    },
                    StreamMessage::MessageDelta { delta, usage } => {
                        if let Some(reason) = delta.stop_reason.as_deref() {
                            self.stop_reason = Some(map_stop_reason(reason)?);
                        }
                        Ok(vec![ProviderEvent::Usage {
                            input_tokens: 0,
                            output_tokens: usage.output_tokens,
                        }])
                    }
                    StreamMessage::MessageStop => {
                        self.saw_message_stop = true;
                        Ok(Vec::new())
                    }
                    StreamMessage::Error { error } => Err(ProviderError::Failed(format!(
                        "anthropic stream error ({}): {}",
                        error.error_type.unwrap_or_else(|| "unknown".to_owned()),
                        error.message
                    ))),
                }
            }
            Some(EventKind::Ping | EventKind::Ignored) | None => Ok(Vec::new()),
        }
    }

    /// Returns the terminal `Finished` event once, after `message_stop` was
    /// seen.
    fn take_pending_terminal(&mut self) -> Result<Option<ProviderEvent>, ProviderError> {
        if !self.saw_message_stop || self.finished_sent {
            return Ok(None);
        }
        let stop_reason = self.stop_reason.ok_or_else(|| {
            ProviderError::Failed("message_stop arrived without a stop_reason".to_owned())
        })?;
        self.finished_sent = true;
        Ok(Some(ProviderEvent::Finished { stop_reason }))
    }
}

/// Strips `field:` (and one optional leading space) from an SSE line,
/// returning the field value when the line carries that field.
fn strip_field<'a>(line: &'a [u8], field: &[u8]) -> Option<&'a [u8]> {
    let (head, rest) = line.split_at_checked(field.len())?;
    if head != field || rest.first() != Some(&b':') {
        return None;
    }
    let value = &rest[1..];
    Some(value.strip_prefix(b" ").unwrap_or(value))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventKind {
    /// Known Anthropic event with a typed payload; parse strictly.
    Known,
    /// `ping`: a known keep-alive with no interesting payload.
    Ping,
    /// A well-formed event Muxi does not consume.
    Ignored,
}

/// Reads the top-level `type` of one `data:` payload to decide how strictly
/// to parse it. `None` means the payload is not a JSON object with a `type`
/// field (malformed), `Some(Ignored)` covers future event kinds.
fn event_kind(text: &str) -> Result<Option<EventKind>, ProviderError> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|error| ProviderError::Failed(format!("malformed stream event: {error}")))?;
    let Some(kind) = value.get("type").and_then(serde_json::Value::as_str) else {
        return Err(ProviderError::Failed(
            "stream event is missing the `type` field".to_owned(),
        ));
    };
    let kind = match kind {
        "message_start" | "content_block_delta" | "message_delta" | "message_stop" | "error" => {
            EventKind::Known
        }
        "ping" => EventKind::Ping,
        _ => EventKind::Ignored,
    };
    Ok(Some(kind))
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
    #[serde(rename = "type", default)]
    error_type: Option<String>,
}

fn map_stop_reason(reason: &str) -> Result<StopReason, ProviderError> {
    let mapped = match reason {
        "end_turn" => StopReason::EndTurn,
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        "pause_turn" => StopReason::PauseTurn,
        "refusal" => StopReason::Refusal,
        "model_context_window_exceeded" => StopReason::ModelContextWindowExceeded,
        other => {
            return Err(ProviderError::Failed(format!(
                "unknown stop_reason: {other}"
            )));
        }
    };
    Ok(mapped)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A complete Anthropic transcript using the given line terminator.
    fn transcript(terminator: &str) -> String {
        [
            "event: message_start",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":12,\"output_tokens\":1}}}",
            "",
            "event: content_block_start",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}",
            "",
            "event: content_block_delta",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}",
            "",
            "event: content_block_stop",
            "data: {\"type\":\"content_block_stop\",\"index\":0}",
            "",
            "event: message_delta",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":34}}",
            "",
            "event: message_stop",
            "data: {\"type\":\"message_stop\"}",
            "",
            "",
        ]
        .join(terminator)
    }

    fn run_decoder(bytes: &[u8]) -> Result<Vec<ProviderEvent>, ProviderError> {
        let mut decoder = SseDecoder::new();
        let mut events = Vec::new();
        for byte in bytes {
            events.extend(decoder.push(std::slice::from_ref(byte))?);
        }
        if let Some(terminal) = decoder.finish()? {
            events.push(terminal);
        }
        Ok(events)
    }

    #[test]
    fn decodes_crlf_anthropic_stream() {
        let events = run_decoder(transcript("\r\n").as_bytes()).expect("decode");
        assert_eq!(
            events,
            vec![
                ProviderEvent::Usage {
                    input_tokens: 12,
                    output_tokens: 1
                },
                ProviderEvent::TextDelta("hello".to_owned()),
                ProviderEvent::Usage {
                    input_tokens: 0,
                    output_tokens: 34
                },
                ProviderEvent::Finished {
                    stop_reason: StopReason::EndTurn
                },
            ]
        );
    }

    #[test]
    fn decodes_lf_anthropic_stream() {
        let events = run_decoder(transcript("\n").as_bytes()).expect("decode");
        assert_eq!(
            events.last(),
            Some(&ProviderEvent::Finished {
                stop_reason: StopReason::EndTurn
            })
        );
    }

    #[test]
    fn decodes_cr_anthropic_stream() {
        let events = run_decoder(transcript("\r").as_bytes()).expect("decode");
        assert_eq!(
            events.last(),
            Some(&ProviderEvent::Finished {
                stop_reason: StopReason::EndTurn
            })
        );
    }

    #[test]
    fn decodes_stream_split_at_every_byte_boundary() {
        let expected = run_decoder(transcript("\r\n").as_bytes()).expect("decode whole");
        let bytes = transcript("\r\n");
        for split in 0..bytes.len() {
            let (head, tail) = bytes.as_bytes().split_at(split);
            let mut decoder = SseDecoder::new();
            let mut events = Vec::new();
            events.extend(decoder.push(head).expect("push head"));
            events.extend(decoder.push(tail).expect("push tail"));
            if let Some(terminal) = decoder.finish().expect("finish") {
                events.push(terminal);
            }
            assert_eq!(events, expected, "split at byte {split}");
        }
    }

    #[test]
    fn preserves_utf8_split_across_chunks() {
        let text = "你好，世界🌍";
        let payload = format!(
            "event: message_start\r\ndata: {{\"type\":\"message_start\",\"message\":{{}}}}\r\n\r\n\
             data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\r\n\r\n\
             event: message_delta\r\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":5}}}}\r\n\r\n\
             event: message_stop\r\ndata: {{\"type\":\"message_stop\"}}\r\n\r\n"
        );
        let events = run_decoder(payload.as_bytes()).expect("decode");
        assert!(events.contains(&ProviderEvent::TextDelta(text.to_owned())));
    }

    #[test]
    fn finishes_only_on_message_stop() {
        let partial = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n\
             data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n";
        let mut decoder = SseDecoder::new();
        let mut events = decoder.push(partial.as_bytes()).expect("push");
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ProviderEvent::Finished { .. }))
        );
        events.extend(
            decoder
                .push(b"data: {\"type\":\"message_stop\"}\n\n")
                .expect("push stop"),
        );
        if let Some(terminal) = decoder.finish().expect("finish") {
            events.push(terminal);
        }
        let finished: Vec<_> = events
            .iter()
            .filter(|event| matches!(event, ProviderEvent::Finished { .. }))
            .collect();
        assert_eq!(finished.len(), 1);
        assert!(events.contains(&ProviderEvent::TextDelta("hi".to_owned())));
    }

    #[test]
    fn rejects_eof_before_message_stop() {
        let result = run_decoder(
            b"data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{}}\n\n",
        );
        assert!(result.is_err());
    }

    #[test]
    fn rejects_truncated_trailing_event() {
        let result = run_decoder(b"data: {\"type\":\"message_stop\"}\n\ndata: {\"type\":\"mess");
        assert!(result.is_err());
    }

    #[test]
    fn rejects_invalid_utf8_payload() {
        let result = run_decoder(b"data: \xff\xfe\n\n");
        assert!(result.is_err());
    }

    #[test]
    fn surfaces_error_event_with_type() {
        let mut decoder = SseDecoder::new();
        let result = decoder.push(
            b"data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
        );
        let error = result.expect_err("error event must fail");
        let text = error.to_string();
        assert!(text.contains("overloaded_error"), "text was: {text}");
        assert!(text.contains("Overloaded"), "text was: {text}");
    }

    #[test]
    fn ignores_ping_and_future_events_before_completion() {
        let events = run_decoder(
            b"event: ping\ndata: {\"type\":\"ping\"}\n\n\
              event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{}}\n\n\
              data: {\"type\":\"message_stop\"}\n\n",
        )
        .expect("decode");
        assert_eq!(
            events,
            vec![
                ProviderEvent::Usage {
                    input_tokens: 0,
                    output_tokens: 0,
                },
                ProviderEvent::Finished {
                    stop_reason: StopReason::EndTurn,
                },
            ]
        );
    }

    #[test]
    fn rejects_malformed_known_event() {
        let result = run_decoder(b"data: {\"type\":\"message_delta\",\"delta\":{}}\n\n");
        assert!(result.is_err(), "known events must parse strictly");
    }

    #[test]
    fn rejects_payload_without_type() {
        let result = run_decoder(b"data: {\"no_type\":true}\n\n");
        assert!(result.is_err());
    }

    #[test]
    fn accepts_data_without_space() {
        let events = run_decoder(
            b"data:{\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{}}\n\ndata:{\"type\":\"message_stop\"}\n\n",
        )
        .expect("decode");
        assert_eq!(
            events,
            vec![
                ProviderEvent::Usage {
                    input_tokens: 0,
                    output_tokens: 0,
                },
                ProviderEvent::Finished {
                    stop_reason: StopReason::EndTurn,
                },
            ]
        );
    }

    #[test]
    fn joins_multiple_data_lines() {
        let events = run_decoder(
            b"data: {\"type\":\ndata: \"ping\"}\n\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{}}\n\ndata: {\"type\":\"message_stop\"}\n\n",
        )
        .expect("decode");
        assert!(matches!(
            events.last(),
            Some(ProviderEvent::Finished {
                stop_reason: StopReason::EndTurn
            })
        ));
    }

    #[test]
    fn ignores_comment_lines_before_completion() {
        let events = run_decoder(
            b": keep-alive\n\ndata: {\"type\":\"ping\"}\n\n\
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{}}\n\n\
              data: {\"type\":\"message_stop\"}\n\n",
        )
        .expect("decode");
        assert!(matches!(
            events.last(),
            Some(ProviderEvent::Finished {
                stop_reason: StopReason::EndTurn
            })
        ));
    }

    #[test]
    fn maps_all_anthropic_stop_reasons() {
        for (wire, expected) in [
            ("end_turn", StopReason::EndTurn),
            ("tool_use", StopReason::ToolUse),
            ("max_tokens", StopReason::MaxTokens),
            ("stop_sequence", StopReason::StopSequence),
            ("pause_turn", StopReason::PauseTurn),
            ("refusal", StopReason::Refusal),
            (
                "model_context_window_exceeded",
                StopReason::ModelContextWindowExceeded,
            ),
        ] {
            assert_eq!(map_stop_reason(wire).expect("map"), expected);
        }
        assert!(map_stop_reason("future_unknown_reason").is_err());
    }

    #[test]
    fn rejects_message_stop_without_stop_reason() {
        let result = run_decoder(b"data: {\"type\":\"message_stop\"}\n\n");
        assert!(result.is_err());
    }
}
