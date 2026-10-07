//! 只观察翻译/兜底收尾之前的上游 SSE；不缓存整轮，也不改变下发字节。

use std::{collections::HashMap, sync::Arc};

use bytes::Bytes;
use futures::{stream::BoxStream, StreamExt};
use serde_json::Value;

use super::usage::{extract_usage, RequestTelemetry};
use crate::server::core::providers::adapter::UpstreamResponse;

const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub enum EvidenceProtocol {
    Chat,
    Anthropic,
    Responses,
    Qoder,
    Accio,
    Trae,
    CodeArts,
}

impl From<UpstreamResponse> for EvidenceProtocol {
    fn from(protocol: UpstreamResponse) -> Self {
        match protocol {
            UpstreamResponse::Chat => Self::Chat,
            UpstreamResponse::Anthropic => Self::Anthropic,
        }
    }
}

/// 必须放在真实上游流上；翻译流中的 [DONE] 可能由网关在 EOF/错误后合成。
pub fn observe_stream(
    stream: BoxStream<'static, Result<Bytes, std::io::Error>>,
    telemetry: Arc<RequestTelemetry>,
    protocol: impl Into<EvidenceProtocol>,
) -> BoxStream<'static, Result<Bytes, std::io::Error>> {
    if !telemetry.observes_affinity() {
        return stream;
    }
    let mut observer = Observer::new(protocol.into());
    stream
        .inspect(move |item| {
            match item {
                Ok(bytes) => observer.push(bytes),
                Err(_) => observer.failed = true,
            }
            telemetry.note_completion_evidence(
                observer.terminal(),
                observer.payload,
                observer.terminal() && observer.input_usage && observer.output_usage,
                observer.failed,
            );
        })
        .boxed()
}

struct Observer {
    protocol: EvidenceProtocol,
    line: Vec<u8>,
    data: String,
    event_type: String,
    oversized: bool,
    stop: bool,
    stop_reason: bool,
    payload: bool,
    input_usage: bool,
    output_usage: bool,
    failed: bool,
    tools: HashMap<String, ToolProof>,
    tool_bytes: usize,
    accio_index: Option<usize>,
}

#[derive(Default)]
struct ToolProof {
    id: String,
    name: String,
    arguments: String,
    fragments: bool,
    closed: bool,
    custom: bool,
}

impl Observer {
    fn new(protocol: EvidenceProtocol) -> Self {
        Self {
            protocol,
            line: Vec::new(),
            data: String::new(),
            event_type: String::new(),
            oversized: false,
            stop: false,
            stop_reason: false,
            payload: false,
            input_usage: false,
            output_usage: false,
            failed: false,
            tools: HashMap::new(),
            tool_bytes: 0,
            accio_index: None,
        }
    }

    fn terminal(&self) -> bool {
        self.stop && (!matches!(self.protocol, EvidenceProtocol::Anthropic) || self.stop_reason)
    }

    fn push(&mut self, bytes: &[u8]) {
        // 每个字节仅扫描一次；UTF-8 在完整行后解析，跨 chunk 不会损坏字符。
        for &byte in bytes {
            if byte == b'\n' {
                if !self.oversized {
                    let line = std::mem::take(&mut self.line);
                    self.consume_line(&line);
                }
                self.line.clear();
                self.oversized = false;
            } else if !self.oversized {
                if self.line.len() == MAX_LINE_BYTES {
                    self.failed = true;
                    self.oversized = true;
                    self.line.clear();
                } else {
                    self.line.push(byte);
                }
            }
        }
        // EOF 不冲刷半行，更不制造真实终态；取消/丢弃同样不会确认。
    }

    fn consume_line(&mut self, line: &[u8]) {
        let Ok(line) = std::str::from_utf8(line) else {
            self.failed = true;
            return;
        };
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            let data = std::mem::take(&mut self.data);
            let kind = std::mem::take(&mut self.event_type);
            self.consume_data(data.trim(), &kind);
        } else if let Some(data) = line.strip_prefix("data:") {
            if self.data.len().saturating_add(data.len()).saturating_add(1) > MAX_LINE_BYTES {
                self.failed = true;
                self.data.clear();
            } else {
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(data.strip_prefix(' ').unwrap_or(data));
            }
        } else if let Some(kind) = line.strip_prefix("event:") {
            self.event_type = kind.trim().to_string();
        }
    }

    fn consume_data(&mut self, data: &str, event_type: &str) {
        // 原生有状态协议的终态不等于它们生成的 Chat [DONE]。
        if matches!(self.protocol, EvidenceProtocol::Qoder) {
            use crate::server::core::providers::qoder::stream::{parse_sse_line, SseEvent};
            match parse_sse_line(data) {
                SseEvent::Done => self.end(),
                SseEvent::Error { .. } => self.failed = true,
                SseEvent::Chunk(chunk) => self.chat(&chunk),
                SseEvent::Skip => {
                    if !data.trim().is_empty() && serde_json::from_str::<Value>(data).is_err() {
                        self.failed = true;
                    }
                }
            }
            return;
        }
        if matches!(self.protocol, EvidenceProtocol::Trae) {
            use crate::server::core::providers::trae::stream::{parse_solo_event, SoloEvent};
            match parse_solo_event(event_type, data) {
                Some(SoloEvent::Done { .. }) => self.end(),
                Some(SoloEvent::Error { .. }) => self.failed = true,
                Some(SoloEvent::TokenUsage(usage)) => self.usage(Some(&usage)),
                Some(SoloEvent::Output {
                    response,
                    tool_calls,
                    ..
                }) => {
                    if !response.is_empty() {
                        self.new_output();
                        self.payload = true;
                    }
                    if let Some(calls) = tool_calls {
                        let calls = crate::server::core::providers::trae::stream::normalize_stream_tool_calls(calls);
                        self.chat_tools(&serde_json::json!({"tool_calls":calls}), "trae");
                    }
                }
                None => self.failed = true,
                Some(SoloEvent::Other) => {}
            }
            return;
        }
        if data.is_empty() {
            return;
        }
        if data == "[DONE]" {
            if matches!(
                self.protocol,
                EvidenceProtocol::Chat | EvidenceProtocol::CodeArts
            ) {
                self.end();
            }
            return;
        }
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            self.failed = true;
            return;
        };
        if !event.is_object() {
            return;
        }
        if event.get("error").is_some_and(|value| !value.is_null()) {
            self.failed = true;
        }
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or(event_type);
        match self.protocol {
            EvidenceProtocol::Chat => {
                self.chat(&event);
            }
            EvidenceProtocol::CodeArts => {
                self.failed |=
                    crate::server::core::providers::codearts::stream_fault::stream_frame_fault(
                        data,
                    )
                    .is_some();
                self.chat(&event);
            }
            EvidenceProtocol::Anthropic => {
                match kind {
                    "error" => self.failed = true,
                    "message_start" => {
                        self.usage(event.pointer("/message/usage"));
                        // 开始帧的 output_tokens=0 是占位，不是本轮最终输出统计。
                        self.output_usage = false;
                    }
                    "message_stop" => self.end(),
                    "message_delta" => {
                        self.usage(event.get("usage"));
                        self.stop_reason |= event
                            .pointer("/delta/stop_reason")
                            .and_then(Value::as_str)
                            .is_some_and(|reason| !reason.trim().is_empty())
                    }
                    "content_block_start" => {
                        let block = event.get("content_block").unwrap_or(&Value::Null);
                        if nonempty(block.get("text")) {
                            self.new_output();
                            self.payload = true;
                        }
                        if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                            self.new_output();
                            let key = format!(
                                "anthropic:{}",
                                event.get("index").and_then(Value::as_i64).unwrap_or(0)
                            );
                            let arguments =
                                block.get("input").map(Value::to_string).unwrap_or_default();
                            self.tool(
                                &key,
                                text(block.get("id")),
                                text(block.get("name")),
                                &arguments,
                                false,
                                false,
                                false,
                            );
                        }
                    }
                    "content_block_delta" => {
                        if nonempty(event.pointer("/delta/text")) {
                            self.new_output();
                            self.payload = true;
                        }
                        if let Some(arguments) =
                            event.pointer("/delta/partial_json").and_then(Value::as_str)
                        {
                            self.new_output();
                            let key = format!(
                                "anthropic:{}",
                                event.get("index").and_then(Value::as_i64).unwrap_or(0)
                            );
                            self.tool(&key, "", "", arguments, true, false, false);
                        }
                    }
                    "content_block_stop" => {
                        let key = format!(
                            "anthropic:{}",
                            event.get("index").and_then(Value::as_i64).unwrap_or(0)
                        );
                        if let Some(tool) = self.tools.get_mut(&key) {
                            tool.closed = true;
                        }
                    }
                    _ => {}
                }
            }
            EvidenceProtocol::Responses => {
                match kind {
                    "error" | "response.failed" | "response.incomplete" => self.failed = true,
                    "response.completed" | "response.done" => {
                        let status = event.pointer("/response/status").and_then(Value::as_str);
                        if status.is_some_and(|status| status != "completed") {
                            self.failed = true;
                        }
                        if let Some(output) =
                            event.pointer("/response/output").and_then(Value::as_array)
                        {
                            for (index, item) in output.iter().enumerate() {
                                self.response_item(&event, item, true, index);
                            }
                        }
                        self.usage(
                            event
                                .get("usage")
                                .or_else(|| event.pointer("/response/usage")),
                        );
                        self.end();
                    }
                    "response.output_text.delta" | "response.refusal.delta" => {
                        if nonempty(event.get("delta")) {
                            self.new_output();
                            self.payload = true;
                        }
                    }
                    "response.output_item.added" | "response.output_item.done" => {
                        if let Some(item) = event.get("item") {
                            self.response_item(&event, item, kind.ends_with(".done"), 0);
                        }
                    }
                    "response.function_call_arguments.delta"
                    | "response.custom_tool_call_input.delta" => {
                        self.new_output();
                        let key = response_key(&event, &Value::Null, 0);
                        self.tool(
                            &key,
                            "",
                            "",
                            text(event.get("delta")),
                            true,
                            false,
                            kind.contains("custom_tool"),
                        );
                    }
                    "response.function_call_arguments.done"
                    | "response.custom_tool_call_input.done" => {
                        let key = response_key(&event, &Value::Null, 0);
                        let argument = event.get("arguments").or_else(|| event.get("input"));
                        self.tool(
                            &key,
                            text(event.get("call_id")),
                            text(event.get("name")),
                            text(argument),
                            false,
                            true,
                            kind.contains("custom_tool"),
                        );
                    }
                    _ => {}
                }
                if !self.stop {
                    self.usage(
                        event
                            .get("usage")
                            .or_else(|| event.pointer("/response/usage")),
                    );
                }
            }
            EvidenceProtocol::Accio => {
                use crate::server::core::providers::accio::stream::{parse_frame, Part};
                if let Some(frame) = parse_frame(data) {
                    self.failed |= frame.is_error();
                    for part in &frame.parts {
                        match part {
                            Part::Text(text) if !text.is_empty() => {
                                self.new_output();
                                self.payload = true;
                            }
                            Part::FunctionCall { id, name, args } => {
                                self.new_output();
                                self.accio_tool(id, name, args);
                            }
                            _ => {}
                        }
                    }
                    if let Some(usage) = event
                        .get("usageMetadata")
                        .or_else(|| event.get("usage_metadata"))
                    {
                        self.failed |= invalid_counts(
                            usage,
                            &[
                                "promptTokenCount",
                                "prompt_token_count",
                                "candidatesTokenCount",
                                "candidates_token_count",
                            ],
                        );
                        self.input_usage |=
                            valid_count(usage, "promptTokenCount", "prompt_token_count");
                        self.output_usage |=
                            valid_count(usage, "candidatesTokenCount", "candidates_token_count");
                    }
                    if frame.turn_complete {
                        self.end();
                    }
                }
            }
            EvidenceProtocol::Qoder | EvidenceProtocol::Trae => {}
        }
    }

    fn chat(&mut self, event: &Value) {
        self.failed |= event.get("error").is_some_and(|value| !value.is_null());
        if let Some(choices) = event.get("choices").and_then(Value::as_array) {
            for choice in choices {
                let delta = choice.get("delta").or_else(|| choice.get("message"));
                if let Some(delta) = delta {
                    if nonempty(delta.get("content")) {
                        self.new_output();
                        self.payload = true;
                    }
                    let choice = choice.get("index").and_then(Value::as_i64).unwrap_or(0);
                    self.chat_tools(delta, &format!("chat:{choice}"));
                }
            }
        }
        self.usage(event.get("usage"));
    }

    fn new_output(&mut self) {
        self.output_usage = false;
        self.failed |= self.stop;
    }

    fn end(&mut self) {
        self.stop = true;
        let require_closed = matches!(
            self.protocol,
            EvidenceProtocol::Anthropic | EvidenceProtocol::Responses
        );
        for tool in self.tools.values() {
            let arguments = if tool.custom {
                crate::server::core::protocol::freeform::wrap_freeform_input(&tool.arguments)
            } else {
                tool.arguments.clone()
            };
            let valid = (matches!(self.protocol, EvidenceProtocol::Accio)
                || !tool.id.trim().is_empty())
                && !tool.name.trim().is_empty()
                && (!require_closed || tool.closed)
                && serde_json::from_str::<Value>(&arguments).is_ok_and(|args| args.is_object());
            self.failed |= !valid;
            self.payload |= valid;
        }
    }

    fn chat_tools(&mut self, delta: &Value, prefix: &str) {
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for (index, call) in calls.iter().enumerate() {
                self.new_output();
                let index = call
                    .get("index")
                    .and_then(Value::as_i64)
                    .unwrap_or(index as i64);
                let key = format!("{prefix}:{index}");
                let function = call.get("function").or_else(|| call.get("function_call"));
                self.tool(
                    &key,
                    text(call.get("id")),
                    text(function.and_then(|function| function.get("name"))),
                    text(function.and_then(|function| function.get("arguments"))),
                    true,
                    false,
                    false,
                );
            }
        }
        if let Some(call) = delta.get("function_call") {
            self.new_output();
            self.tool(
                &format!("{prefix}:legacy"),
                "legacy",
                text(call.get("name")),
                text(call.get("arguments")),
                true,
                false,
                false,
            );
        }
    }

    fn accio_tool(&mut self, id: &str, name: &str, args: &str) {
        // 与 Accio Translator::accumulate_call 同源：无id合法；完整+完整是两个调用。
        let extends = self
            .accio_index
            .and_then(|index| self.tools.get(&format!("accio:{index}")))
            .is_some_and(|last| {
                (id.is_empty() || last.id.is_empty() || id == last.id)
                    && (name.is_empty() || last.name.is_empty() || name == last.name)
                    && !(serde_json::from_str::<Value>(&last.arguments).is_ok()
                        && serde_json::from_str::<Value>(args).is_ok())
            });
        let index = match self.accio_index {
            Some(index) if extends => index,
            Some(index) => index.saturating_add(1),
            None => 0,
        };
        self.accio_index = Some(index);
        self.tool(
            &format!("accio:{index}"),
            id,
            name,
            args,
            true,
            false,
            false,
        );
    }

    fn response_item(&mut self, event: &Value, item: &Value, closed: bool, index: usize) {
        match item.get("type").and_then(Value::as_str) {
            Some("function_call" | "custom_tool_call") => {
                self.new_output();
                let key = response_key(event, item, index);
                let custom = item.get("type").and_then(Value::as_str) == Some("custom_tool_call");
                self.tool(
                    &key,
                    text(item.get("call_id").or_else(|| item.get("id"))),
                    text(item.get("name")),
                    text(item.get("arguments").or_else(|| item.get("input"))),
                    false,
                    closed,
                    custom,
                );
            }
            Some("message") => {
                if response_item_payload(item) {
                    self.new_output();
                    self.payload = true;
                }
            }
            _ => {}
        }
    }

    fn tool(
        &mut self,
        key: &str,
        id: &str,
        name: &str,
        args: &str,
        append: bool,
        closed: bool,
        custom: bool,
    ) {
        // ponytail: 最多256个工具/8MiB元数据与参数；超限只拒绝确认，不改变转发。
        self.tool_bytes = self
            .tool_bytes
            .saturating_add(key.len() + id.len() + name.len() + args.len());
        if self.tool_bytes > MAX_LINE_BYTES
            || (!self.tools.contains_key(key) && self.tools.len() == 256)
        {
            self.failed = true;
            return;
        }
        let tool = self.tools.entry(key.to_string()).or_default();
        if !id.is_empty() {
            tool.id = id.to_string();
        }
        if !name.is_empty() {
            tool.name = name.to_string();
        }
        if append {
            if !tool.fragments {
                tool.arguments.clear();
            }
            tool.arguments.push_str(args);
            tool.fragments = true;
        } else if closed || !args.is_empty() {
            tool.arguments = args.to_string();
        }
        tool.closed |= closed;
        tool.custom |= custom;
    }

    fn usage(&mut self, usage: Option<&Value>) {
        let Some(usage) = usage else {
            return;
        };
        let usage = if [
            "prompt_tokens",
            "input_tokens",
            "completion_tokens",
            "output_tokens",
        ]
        .iter()
        .any(|key| usage.get(key).is_some())
        {
            usage
        } else {
            usage.get("usage").unwrap_or(usage)
        };
        self.failed |= invalid_counts(
            usage,
            &[
                "prompt_tokens",
                "input_tokens",
                "completion_tokens",
                "output_tokens",
            ],
        );
        if let Some(tokens) = extract_usage(usage) {
            self.input_usage |=
                tokens.prompt_present && valid_count(usage, "prompt_tokens", "input_tokens");
            self.output_usage |= tokens.completion_present
                && valid_count(usage, "completion_tokens", "output_tokens");
        }
    }
}

fn invalid_counts(usage: &Value, fields: &[&str]) -> bool {
    fields.iter().filter_map(|key| usage.get(key)).any(|count| {
        !count
            .as_f64()
            .is_some_and(|number| number.is_finite() && number >= 0.0)
    })
}

fn valid_count(usage: &Value, primary: &str, alias: &str) -> bool {
    usage
        .get(primary)
        .or_else(|| usage.get(alias))
        .and_then(Value::as_f64)
        .is_some_and(|number| number.is_finite() && number >= 0.0)
}

fn nonempty(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|text| !text.is_empty())
}

fn text(value: Option<&Value>) -> &str {
    value.and_then(Value::as_str).unwrap_or("")
}

fn response_key(event: &Value, item: &Value, index: usize) -> String {
    let id = text(event.get("item_id").or_else(|| item.get("id")));
    if !id.is_empty() {
        format!("responses:{id}")
    } else {
        format!(
            "responses:index:{}",
            event
                .get("output_index")
                .and_then(Value::as_u64)
                .unwrap_or(index as u64)
        )
    }
}

fn response_item_payload(item: &Value) -> bool {
    match item.get("type").and_then(Value::as_str) {
        Some("message") => item
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|content| {
                content
                    .iter()
                    .any(|part| nonempty(part.get("text")) || nonempty(part.get("refusal")))
            }),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(value: Value) -> Vec<u8> {
        format!("data: {value}\n\n").into_bytes()
    }
    fn complete(observer: &Observer) -> bool {
        observer.terminal()
            && observer.payload
            && observer.input_usage
            && observer.output_usage
            && !observer.failed
    }

    fn active_telemetry() -> (
        Arc<RequestTelemetry>,
        super::super::affinity::Affinity,
        Vec<Value>,
    ) {
        let affinity = super::super::affinity::Affinity::default();
        let pool = vec![serde_json::json!({"id":"A","provider":"workbuddy","uid":"fixture"})];
        let selected = affinity
            .select(
                Some("session"),
                &pool,
                &pool,
                &std::collections::HashMap::new(),
            )
            .unwrap();
        let telemetry = Arc::new(RequestTelemetry::new());
        telemetry.begin_affinity_attempt(affinity.clone(), "A", selected.lease, affinity.epoch());
        (telemetry, affinity, pool)
    }

    #[test]
    fn late_usage_does_not_hold_text_and_split_utf8_remains_valid() {
        let mut observer = Observer::new(EvidenceProtocol::Chat);
        let text = frame(serde_json::json!({"choices":[{"delta":{"content":"正文"}}]}));
        for byte in &text {
            observer.push(std::slice::from_ref(byte));
        }
        assert!(observer.payload);
        assert!(!complete(&observer));
        observer.push(&frame(
            serde_json::json!({"usage":{"prompt_tokens":7,"completion_tokens":2}}),
        ));
        observer.push(b"data: [DONE]\r\n\r\n");
        assert!(complete(&observer));
    }

    #[test]
    fn finish_reason_or_empty_or_partial_usage_never_confirms() {
        for value in [
            serde_json::json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":2}}),
            serde_json::json!({"choices":[{"delta":{"content":"ok"}}],"usage":{"prompt_tokens":7}}),
        ] {
            let mut observer = Observer::new(EvidenceProtocol::Chat);
            observer.push(&frame(value));
            assert!(!complete(&observer));
            observer.push(b"data: [DONE]\n\n");
            assert!(!complete(&observer));
        }
    }

    #[test]
    fn error_followed_by_gateway_done_and_aborted_tail_never_confirms() {
        let mut observer = Observer::new(EvidenceProtocol::Chat);
        observer.push(&frame(serde_json::json!({"choices":[{"delta":{"content":"ok"}}],"usage":{"prompt_tokens":7,"completion_tokens":2}})));
        observer.push(b"data: [DONE]");
        assert!(!complete(&observer));
        observer.push(b"\n");
        assert!(!complete(&observer));
        observer.push(b"\n");
        observer.push(&frame(
            serde_json::json!({"error":{"message":"interrupted"}}),
        ));
        observer.push(b"data: [DONE]\n\n");
        assert!(!complete(&observer));
    }

    #[test]
    fn anthropic_requires_real_stop_reason_and_stop_with_split_usage() {
        let mut observer = Observer::new(EvidenceProtocol::Anthropic);
        observer.push(&frame(
            serde_json::json!({"type":"message_start","message":{"usage":{"input_tokens":7}}}),
        ));
        observer.push(&frame(serde_json::json!({"type":"content_block_start","content_block":{"id":"tool-1","type":"tool_use","name":"lookup","input":{}}})));
        observer.push(&frame(
            serde_json::json!({"type":"content_block_stop","index":0}),
        ));
        observer.push(&frame(serde_json::json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":0}})));
        observer.push(b"data: [DONE]\n\n");
        assert!(!complete(&observer));
        observer.push(&frame(serde_json::json!({"type":"message_stop"})));
        assert!(complete(&observer));
    }

    #[test]
    fn responses_completed_confirms_tools_but_incomplete_or_synthetic_done_does_not() {
        for kind in [
            "response.completed",
            "response.incomplete",
            "response.failed",
        ] {
            let mut observer = Observer::new(EvidenceProtocol::Responses);
            observer.push(&frame(serde_json::json!({"type":kind,"response":{"usage":{"input_tokens":7,"output_tokens":2},"output":[{"id":"tool-1","type":"function_call","name":"lookup","arguments":"{}"}]}})));
            observer.push(b"data: [DONE]\n\n");
            assert_eq!(complete(&observer), kind == "response.completed");
        }
        let mut observer = Observer::new(EvidenceProtocol::Responses);
        observer.push(&frame(serde_json::json!({"type":"response.output_text.delta","delta":"ok","usage":{"input_tokens":7,"output_tokens":2}})));
        observer.push(b"data: [DONE]\n\n");
        assert!(!complete(&observer));
    }

    #[test]
    fn malformed_or_oversized_lines_are_bounded_and_do_not_claim_completion() {
        let mut observer = Observer::new(EvidenceProtocol::Chat);
        observer.push(b"data: {broken}\n\n");
        assert!(observer.failed);
        observer.push(&vec![b'x'; MAX_LINE_BYTES + 1]);
        assert!(observer.line.len() <= MAX_LINE_BYTES);
        observer.push(b"\n");
        assert!(observer.line.is_empty());
        assert!(!complete(&observer));
    }

    #[tokio::test]
    async fn observer_passes_first_bytes_without_waiting_for_usage_or_eof() {
        let first =
            Bytes::from_static(b"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n");
        let source =
            futures::stream::iter(vec![Ok(first.clone())]).chain(futures::stream::pending());
        let (telemetry, affinity, pool) = active_telemetry();
        let mut observed =
            observe_stream(source.boxed(), telemetry.clone(), EvidenceProtocol::Chat);
        let delivered =
            tokio::time::timeout(std::time::Duration::from_secs(1), observed.next()).await;
        assert_eq!(delivered.unwrap().unwrap().unwrap(), first);
        // 下游在完整 usage/终态前 Drop，观察器不会合成任何成功证据。
        drop(observed);
        telemetry.settle_affinity(false);
        assert_eq!(
            affinity
                .select(
                    Some("session"),
                    &pool,
                    &pool,
                    &std::collections::HashMap::new()
                )
                .unwrap()
                .reason,
            "new_session"
        );
    }

    #[test]
    fn qoder_original_done_survives_prefetch_and_business_error_rejects() {
        let mut observer = Observer::new(EvidenceProtocol::Qoder);
        observer.push(&frame(serde_json::json!({"statusCodeValue":200,"body":{"choices":[{"delta":{"content":"ok"}}],"usage":{"prompt_tokens":7,"completion_tokens":2}}})));
        assert!(!complete(&observer));
        observer.push(&frame(
            serde_json::json!({"statusCodeValue":200,"body":"[DONE]"}),
        ));
        assert!(complete(&observer));
        observer.push(&frame(
            serde_json::json!({"statusCodeValue":429,"body":"quota"}),
        ));
        assert!(!complete(&observer));
    }

    #[test]
    fn accio_turn_complete_requires_original_usage_fields_not_translated_defaults() {
        for usage in [
            serde_json::json!({"prompt_token_count":7,"candidates_token_count":0}),
            serde_json::json!({"prompt_token_count":7}),
        ] {
            let expected = usage.get("candidates_token_count").is_some();
            let mut observer = Observer::new(EvidenceProtocol::Accio);
            observer.push(&frame(serde_json::json!({"content":{"parts":[{"function_call":{"id":"tool-1","name":"lookup","args":{}}}]},"usage_metadata":usage})));
            observer.push(b"data: [DONE]\n\n");
            assert!(!complete(&observer));
            observer.push(&frame(serde_json::json!({"turn_complete":true})));
            assert_eq!(complete(&observer), expected);
        }
    }

    #[test]
    fn trae_original_done_is_distinct_from_error_and_eof_done() {
        let mut observer = Observer::new(EvidenceProtocol::Trae);
        let input = b"event: output\ndata: {\"response\":\"ok\"}\n\nevent: token_usage\ndata: {\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2}}\n\n";
        observer.push(input);
        assert!(!complete(&observer));
        let mut synthetic = Observer::new(EvidenceProtocol::Trae);
        synthetic.push(input);
        synthetic.push(b"data: [DONE]\n\n");
        assert!(!complete(&synthetic));
        observer.push(b"event: done\n\n");
        assert!(complete(&observer));
        observer.push(b"event: error\ndata: {\"code\":1005,\"message\":\"quota\"}\n\n");
        assert!(!complete(&observer));
    }

    #[test]
    fn codearts_stream_fault_after_content_cannot_be_confirmed_by_done() {
        let mut observer = Observer::new(EvidenceProtocol::CodeArts);
        observer.push(&frame(serde_json::json!({"choices":[{"delta":{"content":"ok"}}],"usage":{"prompt_tokens":7,"completion_tokens":2}})));
        observer.push(&frame(
            serde_json::json!({"error_code":"InferHub.4291.200","error_msg":"insufficient quota"}),
        ));
        observer.push(b"data: [DONE]\n\n");
        assert!(!complete(&observer));
    }

    #[tokio::test]
    async fn original_stream_evidence_confirms_only_after_final_settlement() {
        for terminal in [true, false] {
            let (telemetry, affinity, pool) = active_telemetry();
            let mut input = frame(
                serde_json::json!({"choices":[{"delta":{"tool_calls":[{"id":"tool-1","function":{"name":"lookup","arguments":"{}"}}]}}],"usage":{"prompt_tokens":7,"completion_tokens":0}}),
            );
            if terminal {
                input.extend_from_slice(b"data: [DONE]\n\n");
            }
            let source = futures::stream::iter([Ok(Bytes::from(input))]).boxed();
            let mut observed = observe_stream(source, telemetry.clone(), EvidenceProtocol::Chat);
            while observed.next().await.is_some() {}
            drop(observed);
            telemetry.settle_affinity(true);
            let next = affinity
                .select(
                    Some("session"),
                    &pool,
                    &pool,
                    &std::collections::HashMap::new(),
                )
                .unwrap();
            assert_eq!(next.reason, if terminal { "sticky" } else { "new_session" });
        }
    }

    #[test]
    fn initial_usage_cannot_replace_final_output_usage_after_payload() {
        let mut chat = Observer::new(EvidenceProtocol::Chat);
        chat.push(&frame(
            serde_json::json!({"usage":{"prompt_tokens":7,"completion_tokens":0}}),
        ));
        chat.push(&frame(
            serde_json::json!({"choices":[{"delta":{"content":"ok"}}]}),
        ));
        chat.push(b"data: [DONE]\n\n");
        assert!(!complete(&chat));

        let mut anthropic = Observer::new(EvidenceProtocol::Anthropic);
        anthropic.push(&frame(serde_json::json!({"type":"message_start","message":{"usage":{"input_tokens":7,"output_tokens":0}}})));
        anthropic.push(&frame(serde_json::json!({"type":"content_block_delta","delta":{"type":"text_delta","text":"ok"}})));
        anthropic.push(&frame(
            serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
        ));
        anthropic.push(&frame(serde_json::json!({"type":"message_stop"})));
        assert!(!complete(&anthropic));
    }

    #[test]
    fn chat_tool_fragments_need_identity_and_complete_object_even_with_text() {
        for valid in [true, false] {
            let mut observer = Observer::new(EvidenceProtocol::Chat);
            observer.push(&frame(serde_json::json!({"choices":[{"delta":{"content":"text","tool_calls":[{"index":0,"id":"tool-1","function":{"name":"lookup","arguments":"{"}}]}}]})));
            if valid {
                observer.push(&frame(serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"}"}}]}}]})));
            }
            observer.push(&frame(
                serde_json::json!({"usage":{"prompt_tokens":7,"completion_tokens":0}}),
            ));
            observer.push(b"data: [DONE]\n\n");
            assert_eq!(complete(&observer), valid);
        }
        let mut anonymous = Observer::new(EvidenceProtocol::Chat);
        anonymous.push(&frame(serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{"}}]}}],"usage":{"prompt_tokens":7,"completion_tokens":2}})));
        anonymous.push(b"data: [DONE]\n\n");
        assert!(!complete(&anonymous));
    }

    #[test]
    fn anthropic_tool_arguments_must_close_and_parse_before_message_stop() {
        for valid in [true, false] {
            let mut observer = Observer::new(EvidenceProtocol::Anthropic);
            observer.push(&frame(serde_json::json!({"type":"message_start","message":{"usage":{"input_tokens":7,"output_tokens":0}}})));
            observer.push(&frame(serde_json::json!({"type":"content_block_start","index":0,"content_block":{"id":"tool-1","type":"tool_use","name":"lookup","input":{}}})));
            observer.push(&frame(serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{"}})));
            if valid {
                observer.push(&frame(serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"}"}})));
            }
            observer.push(&frame(
                serde_json::json!({"type":"content_block_stop","index":0}),
            ));
            observer.push(&frame(serde_json::json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":0}})));
            observer.push(&frame(serde_json::json!({"type":"message_stop"})));
            assert_eq!(complete(&observer), valid);
        }
    }

    #[test]
    fn responses_tool_fragments_require_item_done_and_valid_object() {
        for valid in [true, false] {
            let mut observer = Observer::new(EvidenceProtocol::Responses);
            observer.push(&frame(serde_json::json!({"type":"response.output_item.added","item":{"id":"fc-1","call_id":"call-1","type":"function_call","name":"lookup","arguments":""}})));
            observer.push(&frame(serde_json::json!({"type":"response.function_call_arguments.delta","item_id":"fc-1","delta":"{"})));
            if valid {
                observer.push(&frame(serde_json::json!({"type":"response.function_call_arguments.delta","item_id":"fc-1","delta":"}"})));
                observer.push(&frame(serde_json::json!({"type":"response.function_call_arguments.done","item_id":"fc-1","arguments":"{}"})));
            }
            observer.push(&frame(serde_json::json!({"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":7,"output_tokens":0}}})));
            assert_eq!(complete(&observer), valid);
        }
    }

    #[tokio::test]
    async fn early_complete_placeholder_cannot_be_latched_in_telemetry() {
        let (telemetry, affinity, pool) = active_telemetry();
        let initial = frame(serde_json::json!({"usage":{"prompt_tokens":7,"completion_tokens":0}}));
        let mut rest = frame(serde_json::json!({"choices":[{"delta":{"content":"ok"}}]}));
        rest.extend_from_slice(b"data: [DONE]\n\n");
        let source =
            futures::stream::iter([Ok(Bytes::from(initial)), Ok(Bytes::from(rest))]).boxed();
        let mut observed = observe_stream(source, telemetry.clone(), EvidenceProtocol::Chat);
        while observed.next().await.is_some() {}
        drop(observed);
        telemetry.settle_affinity(true);
        assert_eq!(
            affinity
                .select(
                    Some("session"),
                    &pool,
                    &pool,
                    &std::collections::HashMap::new()
                )
                .unwrap()
                .reason,
            "new_session"
        );
    }

    #[test]
    fn accio_native_tools_allow_missing_id_and_distinguish_fragments_from_calls() {
        for fragments in [vec!["{", "}"], vec!["{}", "{}"]] {
            let mut observer = Observer::new(EvidenceProtocol::Accio);
            for args in fragments {
                observer.push(&frame(serde_json::json!({"content":{"parts":[{"function_call":{"name":"lookup","args_json":args}}]}})));
            }
            observer.push(&frame(serde_json::json!({"turn_complete":true,"usage_metadata":{"prompt_token_count":7,"candidates_token_count":0}})));
            assert!(complete(&observer));
        }
        let mut incomplete = Observer::new(EvidenceProtocol::Accio);
        incomplete.push(&frame(serde_json::json!({"content":{"parts":[{"function_call":{"name":"lookup","args_json":"{"}}]},"turn_complete":true,"usage_metadata":{"prompt_token_count":7,"candidates_token_count":2}})));
        assert!(!complete(&incomplete));
    }

    #[test]
    fn trae_native_single_object_function_call_is_normalized_and_validated() {
        for valid in [true, false] {
            let mut observer = Observer::new(EvidenceProtocol::Trae);
            let output = serde_json::json!({"tool_calls":{"id":"call-1","function_call":{"name":"lookup","arguments":if valid { "{}" } else { "{" },"namespace":"ignored"}}});
            observer.push(format!("event: output\ndata: {output}\n\n").as_bytes());
            observer.push(b"event: token_usage\ndata: {\"prompt_tokens\":7,\"completion_tokens\":0}\n\nevent: done\n\n");
            assert_eq!(complete(&observer), valid);
        }
    }

    #[test]
    fn accio_invalid_final_usage_overrides_earlier_valid_usage() {
        for invalid in [serde_json::json!(-1), serde_json::json!("2"), Value::Null] {
            for (input, output) in [
                ("prompt_token_count", "candidates_token_count"),
                ("promptTokenCount", "candidatesTokenCount"),
            ] {
                for field in [input, output] {
                    let mut observer = Observer::new(EvidenceProtocol::Accio);
                    observer.push(&frame(
                        serde_json::json!({"content":{"parts":[{"text":"ok"}]}}),
                    ));
                    observer.push(&frame(serde_json::json!({"usage_metadata":{"prompt_token_count":7,"candidates_token_count":2}})));
                    let mut usage = serde_json::json!({input:7, output:2});
                    usage[field] = invalid.clone();
                    observer.push(&frame(
                        serde_json::json!({"turn_complete":true,"usage_metadata":usage}),
                    ));
                    assert!(!complete(&observer), "invalid final {field}: {invalid}");
                }
            }
        }
    }
}
