//! SSE framing, aggregation and `previous_response_id` rewrite. The ChatGPT
//! Codex backend only ever answers with `text/event-stream`, so subbier always
//! asks upstream for SSE and reassembles a single JSON response for clients
//! that did not ask to stream.

use serde_json::Value;
use std::collections::BTreeMap;

/// Longest first: `\r\n` must win over a bare `\r` so that `\r\n\r\n` is one
/// separator rather than two frames' worth of line breaks.
const LINE_BREAKS: [&[u8]; 3] = [b"\r\n", b"\r", b"\n"];

/// The longest separator is `\r\n\r\n`, so at most three trailing bytes can be
/// a partial separator that a later chunk completes.
const MAX_SEPARATOR_LEN: usize = 4;

fn line_break_at(buf: &[u8], i: usize) -> Option<usize> {
    LINE_BREAKS
        .iter()
        .find(|brk| buf[i..].starts_with(brk))
        .map(|brk| brk.len())
}

/// Length of the frame separator starting at `i`: two line terminators, each
/// of which may be `\r\n`, `\r` or `\n`. Matched greedily and without
/// backtracking, so a lone `\r\n` stays a line terminator.
fn separator_at(buf: &[u8], i: usize) -> Option<usize> {
    let first = line_break_at(buf, i)?;
    let second = line_break_at(buf, i + first)?;
    Some(first + second)
}

/// Incremental SSE framer: feed it bytes, get whole frames out, without their
/// trailing separator.
#[derive(Debug, Default)]
pub struct SseFramer {
    buf: Vec<u8>,
    /// Everything before this offset cannot begin a separator.
    scan_from: usize,
}

impl SseFramer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append `chunk` and return every frame it completed. Buffering bytes
    /// rather than text survives a chunk boundary that falls inside a
    /// multi-byte character; a separator byte never occurs inside one.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut frames = Vec::new();
        let mut frame_start = 0usize;
        let mut i = self.scan_from;
        while i < self.buf.len() {
            match separator_at(&self.buf, i) {
                Some(len) => {
                    frames.push(decode(&self.buf[frame_start..i]));
                    i += len;
                    frame_start = i;
                }
                None => i += 1,
            }
        }
        self.buf.drain(..frame_start);
        self.scan_from = self.buf.len().saturating_sub(MAX_SEPARATOR_LEN - 1);
        frames
    }

    /// The trailing, unterminated frame at end of stream.
    pub fn flush(&mut self) -> Option<String> {
        self.scan_from = 0;
        if self.buf.is_empty() {
            return None;
        }
        let frame = decode(&self.buf);
        self.buf.clear();
        Some(frame)
    }
}

fn decode(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The concatenated `data:` payload of one frame, or `None` for `[DONE]`,
/// empty and comment/`event:`/`id:`-only frames.
pub fn data_payload(frame: &str) -> Option<String> {
    let mut payload = String::new();
    let mut any = false;
    for line in split_lines(frame) {
        if let Some(rest) = line.strip_prefix("data:") {
            if any {
                payload.push('\n');
            }
            payload.push_str(rest.trim_start());
            any = true;
        }
    }
    let payload = payload.trim();
    if payload.is_empty() || payload == "[DONE]" {
        return None;
    }
    Some(payload.to_string())
}

/// Split on single line terminators (`\r\n`, `\r` or `\n`), without the
/// spurious empty line a plain `split` yields per CRLF.
fn split_lines(frame: &str) -> impl Iterator<Item = &str> {
    let mut rest = frame;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        match rest.find(['\r', '\n']) {
            Some(idx) => {
                let line = &rest[..idx];
                let skip = if rest[idx..].starts_with("\r\n") {
                    2
                } else {
                    1
                };
                rest = &rest[idx + skip..];
                Some(line)
            }
            None => {
                let line = rest;
                rest = "";
                Some(line)
            }
        }
    })
}

/// What a whole upstream stream added up to.
#[derive(Debug, Clone, PartialEq)]
pub enum Aggregated {
    /// The terminal `response` object, ready to hand back as JSON.
    Terminal(Value),
    /// The stream ended without `response.completed`, `response.incomplete` or
    /// `response.failed`. `detail` is the captured `error` event, truncated.
    NoTerminal { detail: Option<String> },
}

impl Aggregated {
    pub const NO_TERMINAL_MESSAGE: &'static str =
        "upstream stream ended without a terminal response event";

    /// `None` for a terminal response, which is not an error.
    pub fn error_message(&self) -> Option<String> {
        match self {
            Self::Terminal(_) => None,
            Self::NoTerminal { detail: None } => Some(Self::NO_TERMINAL_MESSAGE.to_string()),
            Self::NoTerminal {
                detail: Some(detail),
            } => Some(format!("{}: {detail}", Self::NO_TERMINAL_MESSAGE)),
        }
    }
}

const ERROR_DETAIL_LIMIT: usize = 300;

/// Folds SSE frames into one terminal response object.
#[derive(Debug, Default)]
pub struct Aggregator {
    terminal: Option<Value>,
    error: Option<Value>,
    items: BTreeMap<u64, Value>,
    next_item_index: u64,
}

impl Aggregator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Unparseable and non-`data:` frames are ignored: upstream is entitled to
    /// send comments and keep-alives.
    pub fn consume(&mut self, frame: &str) {
        if self.terminal.is_some() {
            return;
        }
        let Some(payload) = data_payload(frame) else {
            return;
        };
        let Ok(event) = serde_json::from_str::<Value>(&payload) else {
            return;
        };
        let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");

        if event_type == "response.output_item.done"
            && let Some(item) = event.get("item")
        {
            let index = output_index(event.get("output_index")).unwrap_or(self.next_item_index);
            self.items.insert(index, item.clone());
            self.next_item_index = self.next_item_index.max(index.saturating_add(1));
        }

        if matches!(
            event_type,
            "response.completed" | "response.incomplete" | "response.failed"
        ) && let Some(response) = event.get("response")
            && response.is_object()
        {
            self.terminal = Some(response.clone());
        }

        if event_type == "error" {
            self.error = Some(event);
        }
    }

    /// A terminal event has arrived. The caller must stop reading and cancel
    /// the reader: waiting for `[DONE]` or EOF hangs against servers that keep
    /// the connection open.
    pub fn is_done(&self) -> bool {
        self.terminal.is_some()
    }

    /// Backfills an empty `output` from the collected
    /// `response.output_item.done` items, in `output_index` order.
    pub fn finish(self) -> Aggregated {
        let Some(mut terminal) = self.terminal else {
            let detail = self
                .error
                .as_ref()
                .map(|error| truncate(&error.to_string()));
            return Aggregated::NoTerminal { detail };
        };
        // A terminal response that carries its own output is authoritative.
        let needs_output = terminal
            .get("output")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty);
        if needs_output && let Some(object) = terminal.as_object_mut() {
            let output = self.items.into_values().collect::<Vec<_>>();
            object.insert("output".to_string(), Value::Array(output));
        }
        Aggregated::Terminal(terminal)
    }
}

/// A usable `output_index`: a non-negative integer, or a float that happens to
/// be one.
fn output_index(value: Option<&Value>) -> Option<u64> {
    let number = value?.as_number()?;
    if let Some(index) = number.as_u64() {
        return Some(index);
    }
    let float = number.as_f64()?;
    (float.is_finite() && float >= 0.0 && float.fract() == 0.0).then_some(float as u64)
}

fn truncate(text: &str) -> String {
    match text.char_indices().nth(ERROR_DETAIL_LIMIT) {
        Some((end, _)) => text[..end].to_string(),
        None => text.to_string(),
    }
}

/// Put `id` back into `response.previous_response_id` inside one SSE frame.
///
/// A streamed chain has no aggregation step to re-attach the client's id to —
/// it was inlined into `input` before the request went upstream — so every
/// frame carrying a `response` object gets it written back. A multi-line
/// `data:` payload collapses into the first `data:` line.
pub fn rewrite_previous_response_id(frame: &str, id: &str) -> String {
    let Some(payload) = data_payload(frame) else {
        return frame.to_string();
    };
    let Ok(mut event) = serde_json::from_str::<Value>(&payload) else {
        return frame.to_string();
    };
    let Some(response) = event.get_mut("response").and_then(Value::as_object_mut) else {
        return frame.to_string();
    };
    response.insert(
        "previous_response_id".to_string(),
        Value::String(id.to_string()),
    );

    let rewritten = format!("data: {event}");
    let mut replaced = false;
    let mut out = Vec::new();
    for line in split_lines(frame) {
        if line.starts_with("data:") {
            if replaced {
                continue;
            }
            replaced = true;
            out.push(rewritten.as_str());
        } else {
            out.push(line);
        }
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn frames(chunks: &[&str]) -> Vec<String> {
        let mut framer = SseFramer::new();
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend(framer.push(chunk.as_bytes()));
        }
        out.extend(framer.flush());
        out
    }

    #[test]
    fn frames_split_on_any_pair_of_line_breaks() {
        for (stream, expected) in [
            ("data: a\n\ndata: b\n\n", &["data: a", "data: b"][..]),
            (
                "data: a\r\n\r\ndata: b\r\n\r\n",
                &["data: a", "data: b"][..],
            ),
            ("data: a\r\rdata: b\r\r", &["data: a", "data: b"][..]),
            (
                "data: a\n\rdata: b\r\n\ndata: c\n\r\n",
                &["data: a", "data: b", "data: c"][..],
            ),
        ] {
            assert_eq!(frames(&[stream]), expected, "{stream:?}");
        }
    }

    /// A lone CRLF is a line terminator, not a frame boundary.
    #[test]
    fn a_lone_crlf_does_not_split_a_frame() {
        assert_eq!(
            frames(&["event: x\r\ndata: {\"a\":\r\ndata: 1}\r\n\r\n"]),
            ["event: x\r\ndata: {\"a\":\r\ndata: 1}"]
        );
        assert_eq!(
            data_payload("event: x\r\ndata: {\"a\":\r\ndata: 1}").as_deref(),
            Some("{\"a\":\n1}")
        );
    }

    #[test]
    fn frames_reassemble_across_push_boundaries() {
        let mut framer = SseFramer::new();
        assert!(framer.push(b"data: {\"ty").is_empty());
        assert!(framer.push(b"pe\":\"ping\"}\r").is_empty());
        assert_eq!(framer.push(b"\n\r\n"), ["data: {\"type\":\"ping\"}"]);
        assert_eq!(framer.flush(), None);

        // The separator itself, split down the middle.
        let mut framer = SseFramer::new();
        assert!(framer.push(b"data: a\r\n").is_empty());
        assert_eq!(framer.push(b"\r\ndata: b"), ["data: a"]);
        assert_eq!(framer.flush().as_deref(), Some("data: b"));
        assert_eq!(framer.flush(), None);
    }

    #[test]
    fn a_multibyte_character_split_across_chunks_is_not_mangled() {
        let text = "data: {\"emoji\":\"🎈é\"}\n\n";
        let bytes = text.as_bytes();
        // Cut inside the four-byte balloon.
        let cut = bytes.iter().position(|b| *b == 0xF0).unwrap() + 2;
        let mut framer = SseFramer::new();
        assert!(framer.push(&bytes[..cut]).is_empty());
        assert_eq!(framer.push(&bytes[cut..]), ["data: {\"emoji\":\"🎈é\"}"]);
    }

    #[test]
    fn data_payload_concatenates_data_lines_and_ignores_the_rest() {
        for (frame, expected) in [
            ("data: one\ndata: two\ndata: three", Some("one\ntwo\nthree")),
            (
                "event: response.completed\nid: 7\ndata: {\"a\":1}\n: comment",
                Some("{\"a\":1}"),
            ),
            ("data:  padded  ", Some("padded")),
            ("data:tight", Some("tight")),
            ("data: [DONE]", None),
            ("data:", None),
            ("", None),
            (": keep-alive", None),
        ] {
            assert_eq!(data_payload(frame).as_deref(), expected, "{frame:?}");
        }
    }

    fn terminal_frame(status: &str, response: Value) -> String {
        format!(
            "data: {}",
            json!({ "type": format!("response.{status}"), "response": response })
        )
    }

    fn item_frame(index: Value, item: Value) -> String {
        format!(
            "data: {}",
            json!({ "type": "response.output_item.done", "output_index": index, "item": item })
        )
    }

    #[test]
    fn every_terminal_event_type_terminates() {
        for status in ["completed", "incomplete", "failed"] {
            let mut agg = Aggregator::new();
            agg.consume(&terminal_frame(
                status,
                json!({ "id": "resp_1", "status": status, "output": [] }),
            ));
            assert!(agg.is_done(), "{status} should terminate");
            let Aggregated::Terminal(response) = agg.finish() else {
                panic!("{status} should aggregate to a terminal response");
            };
            assert_eq!(response["status"], status);
            assert_eq!(response["id"], "resp_1");
        }
    }

    /// The caller must bail out with bytes still buffered, never wait for a `[DONE]` that never arrives.
    #[test]
    fn is_done_flips_before_the_stream_ends() {
        let payload = format!(
            "{}\n\n{}\n\ndata: [DONE]\n\n",
            item_frame(json!(0), json!({ "type": "message" })),
            terminal_frame("completed", json!({ "id": "resp_1", "output": [] })),
        );
        let mut framer = SseFramer::new();
        let mut agg = Aggregator::new();
        let mut consumed = 0;
        for frame in framer.push(payload.as_bytes()) {
            if agg.is_done() {
                break;
            }
            agg.consume(&frame);
            consumed += 1;
        }
        assert!(agg.is_done());
        assert_eq!(consumed, 2, "stopped before [DONE]");
    }

    #[test]
    fn an_empty_terminal_output_is_filled_in_output_index_order() {
        let mut agg = Aggregator::new();
        agg.consume(&item_frame(
            json!(1),
            json!({ "type": "message", "content": "second" }),
        ));
        agg.consume(&item_frame(
            json!(0),
            json!({ "type": "message", "content": "first" }),
        ));
        agg.consume(&terminal_frame(
            "completed",
            json!({ "id": "resp_1", "output": [] }),
        ));
        let Aggregated::Terminal(response) = agg.finish() else {
            panic!("expected a terminal response");
        };
        let contents: Vec<&str> = response["output"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["content"].as_str().unwrap())
            .collect();
        assert_eq!(contents, ["first", "second"]);
    }

    #[test]
    fn a_missing_output_index_falls_back_to_arrival_order() {
        let mut agg = Aggregator::new();
        agg.consume("data: {\"type\":\"response.output_item.done\",\"item\":{\"n\":0}}");
        agg.consume(&item_frame(json!(-1), json!({ "n": 1 })));
        agg.consume(&item_frame(json!("nope"), json!({ "n": 2 })));
        agg.consume(&item_frame(json!(9), json!({ "n": 9 })));
        agg.consume(&terminal_frame("completed", json!({ "id": "resp_1" })));
        let Aggregated::Terminal(response) = agg.finish() else {
            panic!("expected a terminal response");
        };
        let ns: Vec<u64> = response["output"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["n"].as_u64().unwrap())
            .collect();
        assert_eq!(ns, [0, 1, 2, 9]);
    }

    #[test]
    fn a_nonempty_terminal_output_is_left_alone() {
        let mut agg = Aggregator::new();
        agg.consume(&item_frame(json!(0), json!({ "from": "item" })));
        agg.consume(&terminal_frame(
            "completed",
            json!({ "id": "resp_1", "output": [{ "from": "terminal" }] }),
        ));
        let Aggregated::Terminal(response) = agg.finish() else {
            panic!("expected a terminal response");
        };
        assert_eq!(response["output"], json!([{ "from": "terminal" }]));
    }

    #[test]
    fn frames_after_the_terminal_event_are_ignored() {
        let mut agg = Aggregator::new();
        agg.consume(&terminal_frame(
            "completed",
            json!({ "id": "resp_1", "output": [] }),
        ));
        agg.consume(&terminal_frame(
            "failed",
            json!({ "id": "resp_2", "output": [] }),
        ));
        let Aggregated::Terminal(response) = agg.finish() else {
            panic!("expected a terminal response");
        };
        assert_eq!(response["id"], "resp_1");
    }

    #[test]
    fn a_stream_without_a_terminal_event_reports_what_it_saw() {
        let mut agg = Aggregator::new();
        agg.consume("data: {\"type\":\"error\",\"code\":\"boom\",\"message\":\"upstream sad\"}");
        assert!(!agg.is_done());
        let aggregated = agg.finish();
        let Aggregated::NoTerminal {
            detail: Some(detail),
        } = &aggregated
        else {
            panic!("expected a captured error detail, got {aggregated:?}");
        };
        assert!(detail.contains("boom"), "{detail}");
        assert!(
            aggregated
                .error_message()
                .unwrap()
                .starts_with(Aggregated::NO_TERMINAL_MESSAGE)
        );

        let mut agg = Aggregator::new();
        agg.consume("data: [DONE]");
        agg.consume(": keep-alive");
        let aggregated = agg.finish();
        assert_eq!(aggregated, Aggregated::NoTerminal { detail: None });
        assert_eq!(
            aggregated.error_message().as_deref(),
            Some(Aggregated::NO_TERMINAL_MESSAGE)
        );
    }

    #[test]
    fn a_long_error_detail_is_truncated() {
        let mut agg = Aggregator::new();
        agg.consume(&format!(
            "data: {}",
            json!({ "type": "error", "message": "é".repeat(500) })
        ));
        let Aggregated::NoTerminal {
            detail: Some(detail),
        } = agg.finish()
        else {
            panic!("expected a captured error detail");
        };
        assert_eq!(detail.chars().count(), ERROR_DETAIL_LIMIT);
    }

    #[test]
    fn rewrite_sets_previous_response_id_and_keeps_other_lines() {
        let frame = format!(
            "event: response.completed\ndata: {}\nid: 7",
            json!({ "type": "response.completed", "response": { "id": "resp_2", "previous_response_id": null } })
        );
        let out = rewrite_previous_response_id(&frame, "resp_1");
        let mut lines = out.split('\n');
        assert_eq!(lines.next(), Some("event: response.completed"));
        let data = lines.next().unwrap();
        assert_eq!(lines.next(), Some("id: 7"));
        assert_eq!(lines.next(), None);
        let event: Value = serde_json::from_str(data.strip_prefix("data: ").unwrap()).unwrap();
        assert_eq!(event["response"]["previous_response_id"], "resp_1");
        assert_eq!(event["response"]["id"], "resp_2");
    }

    #[test]
    fn rewrite_collapses_a_multiline_payload_into_the_first_data_line() {
        let frame = "event: x\ndata: {\"response\":\ndata: {\"id\":\"resp_2\"}}\nid: 7";
        let out = rewrite_previous_response_id(frame, "resp_1");
        let lines: Vec<&str> = out.split('\n').collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "event: x");
        assert!(lines[1].starts_with("data: {"), "{}", lines[1]);
        assert_eq!(lines[2], "id: 7");
        let event: Value = serde_json::from_str(lines[1].strip_prefix("data: ").unwrap()).unwrap();
        assert_eq!(event["response"]["previous_response_id"], "resp_1");
    }

    #[test]
    fn rewrite_leaves_frames_it_cannot_touch_alone() {
        for frame in [
            "data: [DONE]",
            ": keep-alive",
            "data: not json",
            "data: {\"type\":\"response.output_item.done\",\"item\":{}}",
            "data: {\"response\":\"not an object\"}",
        ] {
            assert_eq!(rewrite_previous_response_id(frame, "resp_1"), frame);
        }
    }
}
