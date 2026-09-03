//! The Claude path: `POST /v1/messages`, `/v1/messages/count_tokens` and `GET /v1/models`.
//! Anthropic's API is stateless, so unlike [`super::codex`] there is no aggregation and no
//! transcript emulation. The one rewrite is [`ensure_identity_first`]: an OAuth bearer is
//! accepted only with the identity string *first* in `system`, and the rejection to that
//! comes disguised as a `429 rate_limit_error`, which must not be read as exhaustion.

use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{Map, Value};
use tracing::Instrument;

use crate::balance::{self, Attempt, Disposition, FailureClass, Hints};
use crate::model::{Provider, Sub, SubId, Usage};
use crate::provider::claude::parse_unified_headers;

use super::codex::request_scoped;
use super::sse::{self, SseFramer};
use super::{
    ORIGINATOR, ProxyState, Rejection, RequestRecord, SubEntry, body_excerpt, error_response,
    is_json_media_type, passthrough_headers, read_body, reject, send_retrying, upstream,
    usage_for_exhaustion,
};

/// Verbatim: the OAuth scope is issued for this exact string, and changing so much as the
/// punctuation reintroduces the disguised 429.
pub const IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// The beta flag an OAuth bearer must be presented with; the floor, not the whole set.
pub const ANTHROPIC_BETA: &str = "oauth-2025-04-20";

pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The one client header [`send`] merges rather than drops: `anthropic-beta` is client-driven
/// feature negotiation, so dropping it desynchronises the body from the header that permits it
/// (forwarding `context_management` without its beta earns a `400 Extra inputs are not
/// permitted`). Ours first, de-duplicated.
fn merged_beta(headers: &axum::http::HeaderMap) -> String {
    let mut merged = vec![ANTHROPIC_BETA.to_owned()];
    for value in headers.get_all("anthropic-beta") {
        let Ok(value) = value.to_str() else { continue };
        for token in value.split(',') {
            let token = token.trim();
            if token.is_empty() || merged.iter().any(|seen| seen.eq_ignore_ascii_case(token)) {
                continue;
            }
            merged.push(token.to_owned());
        }
    }
    merged.join(",")
}

#[must_use]
pub fn messages_url(base: &str) -> String {
    format!("{}/v1/messages", base.trim_end_matches('/'))
}

#[must_use]
pub fn count_tokens_url(base: &str) -> String {
    format!("{}/v1/messages/count_tokens", base.trim_end_matches('/'))
}

#[must_use]
pub fn models_url(base: &str) -> String {
    format!("{}/v1/models", base.trim_end_matches('/'))
}

fn text_block(text: &str) -> Value {
    serde_json::json!({ "type": "text", "text": text })
}

/// Both spellings the API accepts: a `{"type":"text","text":…}` block and a bare string.
fn is_identity_block(value: &Value) -> bool {
    match value {
        Value::String(text) => text.trim() == IDENTITY,
        Value::Object(block) => block
            .get("text")
            .and_then(Value::as_str)
            .is_some_and(|text| text.trim() == IDENTITY),
        _ => false,
    }
}

/// Make [`IDENTITY`] the first element of `system`, keeping the caller's blocks after it; an
/// identity already present but later is moved, not duplicated. Returns whether it changed.
pub fn ensure_identity_first(body: &mut Map<String, Value>) -> bool {
    let original = body.get("system").cloned();
    let mut blocks: Vec<Value> = match original.clone() {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(blocks)) => blocks,
        Some(Value::String(text)) if text.is_empty() => Vec::new(),
        Some(Value::String(text)) => vec![text_block(&text)],
        // Neither spelling the API defines: a clean upstream 400 beats the disguised 429 that
        // dropping the identity would earn.
        Some(other) => vec![other],
    };

    match blocks.iter().position(is_identity_block) {
        Some(0) => {}
        Some(index) => {
            let identity = blocks.remove(index);
            blocks.insert(0, identity);
        }
        None => blocks.insert(0, text_block(IDENTITY)),
    }

    let rebuilt = Value::Array(blocks);
    let changed = original.as_ref() != Some(&rebuilt);
    body.insert("system".into(), rebuilt);
    changed
}

/// Token counts off an Anthropic `usage` block, in all three shapes it arrives in: a Messages
/// response, a streaming payload, and a `count_tokens` body, which is a bare
/// `{"input_tokens":…}` with no envelope and no output count.
///
/// `cache_creation_input_tokens` and `cache_read_input_tokens` are *added* to the input count:
/// Anthropic reports them alongside `input_tokens` rather than inside it.
#[must_use]
pub fn token_counts(value: &Value) -> (Option<u64>, Option<u64>) {
    let usage = value.get("usage").unwrap_or(value);
    let mut input: Option<u64> = None;
    for field in [
        "input_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
    ] {
        if let Some(count) = usage.get(field).and_then(Value::as_u64) {
            input = Some(input.unwrap_or(0) + count);
        }
    }
    let output = usage.get("output_tokens").and_then(Value::as_u64);
    (input, output)
}

/// `message_start` nests `usage` under `message`; `message_delta` puts it at
/// the top level of the event.
fn stream_usage(event: &Value) -> Option<&Value> {
    match event.get("type").and_then(Value::as_str) {
        Some("message_start") => event.get("message"),
        Some("message_delta") => Some(event),
        _ => None,
    }
}

/// Feed the `Anthropic-Ratelimit-Unified-*` headers into the usage cache and quarantine on
/// them: the cut-off verdict is never in the `/api/oauth/usage` body, only in these headers.
/// Returns the snapshot so a usage-limit classification can skip a forced refetch.
fn observe_rate_limits(
    state: &ProxyState,
    entry: &SubEntry,
    headers: &reqwest::header::HeaderMap,
) -> Option<Usage> {
    let usage = parse_unified_headers(headers)?.to_usage()?;
    state.usage.observe(entry.key(), usage.clone());
    if balance::is_exhausted(&usage) {
        let until = state.router.exhaust(entry.id, Some(&usage));
        tracing::info_span!(
            "sub.exhausted",
            sub = %entry.key(),
            until = %until,
            cause = "unified-ratelimit-headers",
        )
        .in_scope(|| tracing::info!("quarantined"));
    }
    Some(usage)
}

/// One upstream call: the three routes differ only in method, URL, body and streaming, so
/// they share one failover loop.
struct Forward {
    method: reqwest::Method,
    url: String,
    body: Option<Vec<u8>>,
    /// Forwarded exactly as it arrived; unlike the Codex path there is nothing to force.
    client_stream: bool,
    accept: &'static str,
    /// Ours unioned with the client's; see [`merged_beta`].
    beta: String,
}

pub async fn messages(State(state): State<Arc<ProxyState>>, request: Request) -> Response {
    let url = messages_url(state.base(Provider::Claude));
    body_route(state, request, url, true).await
}

/// Shares `system` with [`messages`], so it gets the same identity treatment — which also
/// keeps its answer consistent with what the matching `/v1/messages` call is billed for.
pub async fn count_tokens(State(state): State<Arc<ProxyState>>, request: Request) -> Response {
    let url = count_tokens_url(state.base(Provider::Claude));
    body_route(state, request, url, false).await
}

/// Only under the explicit `/anthropic` alias: the bare `/v1/models` is the Codex catalog.
pub async fn models(State(state): State<Arc<ProxyState>>, request: Request) -> Response {
    let route = request.uri().path().to_owned();
    let forward = Forward {
        method: reqwest::Method::GET,
        url: models_url(state.base(Provider::Claude)),
        body: None,
        client_stream: false,
        accept: "application/json",
        beta: merged_beta(request.headers()),
    };
    forward_with_failover(state, route, forward).await
}

async fn body_route(
    state: Arc<ProxyState>,
    request: Request,
    url: String,
    streamable: bool,
) -> Response {
    // A browser-safelisted content type must not be able to drive a proxy holding OAuth tokens.
    if !is_json_media_type(request.headers()) {
        return error_response(415, "content-type must be application/json");
    }
    let route = request.uri().path().to_owned();
    let (parts, body) = request.into_parts();
    let beta = merged_beta(&parts.headers);
    let raw = match read_body(body).await {
        Ok(raw) => raw,
        Err(rejection) => return *rejection,
    };
    let (body, client_stream) = match prepare(&raw) {
        Ok(prepared) => prepared,
        Err(rejection) => return *rejection,
    };
    let client_stream = client_stream && streamable;

    let forward = Forward {
        method: reqwest::Method::POST,
        url,
        body: Some(body),
        client_stream,
        accept: if client_stream {
            "text/event-stream"
        } else {
            "application/json"
        },
        beta,
    };
    forward_with_failover(state, route, forward).await
}

fn prepare(raw: &[u8]) -> Result<(Vec<u8>, bool), Rejection> {
    let value: Value = serde_json::from_slice(raw).map_err(|_| reject(400, "invalid JSON body"))?;
    let Value::Object(mut body) = value else {
        return Err(reject(400, "JSON body must be an object"));
    };

    ensure_identity_first(&mut body);
    let client_stream = body.get("stream") == Some(&Value::Bool(true));

    let serialised = serde_json::to_vec(&body)
        .map_err(|e| reject(500, format!("could not re-encode the request body: {e}")))?;
    Ok((serialised, client_stream))
}

enum Outcome {
    /// Hand this to the client and stop.
    Done(Response),
    /// Try the next candidate; `terminal` is the status if every candidate is used up.
    Rotate { terminal: u16 },
    /// Stop now with this response; do not touch another sub.
    Fail(Response),
}

async fn forward_with_failover(
    state: Arc<ProxyState>,
    route: String,
    forward: Forward,
) -> Response {
    // Resolved once: failover must stay inside the pool the request arrived on, whatever the
    // registry does mid-failover.
    let pool = match crate::proxy::pool_from_path(&route).map(|name| state.pool_gate(name)) {
        Some(Ok(gate)) => Some(gate),
        Some(Err(e)) => {
            state.note_error(&e);
            return error_response(e.status(), e);
        }
        None => None,
    };

    let mut terminal_status = 429u16;
    let mut max_attempts = usize::MAX;
    let mut attempt_no = 0usize;

    while attempt_no < max_attempts {
        let statuses = state
            .subs
            .statuses(Provider::Claude, &state.metrics, Some(&state.usage));
        let scorer = state.scorer(Provider::Claude);
        let select_span = tracing::info_span!(
            "balance.select",
            provider = "claude",
            pool = pool.as_ref().map(|p| p.name.as_str()).unwrap_or("-"),
            strategy = %state.router.settings().strategy,
            candidates = tracing::field::Empty,
            chosen = tracing::field::Empty,
            reason = tracing::field::Empty,
            usage_round = tracing::field::Empty,
        );
        // Anthropic hands out no ids to chain from and takes no cache key, so there are no hints.
        let selection = state
            .router
            .select_in(
                Provider::Claude,
                &statuses,
                Hints::default(),
                pool.as_ref(),
                &scorer,
            )
            .instrument(select_span.clone())
            .await;
        let selection = match selection {
            Ok(selection) => selection,
            Err(e) => {
                state.note_error(&e);
                return error_response(e.status(), e);
            }
        };
        select_span.record("candidates", selection.candidates);
        select_span.record("chosen", selection.sub.0);
        select_span.record("reason", tracing::field::debug(selection.reason));
        select_span.record(
            "usage_round",
            matches!(selection.reason, crate::balance::SelectReason::Strategy(_)),
        );

        // At most one attempt per candidate: rotating further revisits a sub that already failed.
        if attempt_no == 0 {
            max_attempts = selection.candidates.max(1);
        }
        attempt_no += 1;

        let Some(entry) = state.subs.get(selection.sub) else {
            // The engine replaced the registry between select and fetch.
            continue;
        };

        let span = tracing::info_span!(
            "proxy.request",
            provider = "claude",
            route = %route,
            attempt = attempt_no,
            sub = %entry.key(),
            reason = tracing::field::debug(selection.reason),
            status = tracing::field::Empty,
            duration_ms = tracing::field::Empty,
        );
        let outcome = attempt(&state, &entry, &route, &forward)
            .instrument(span)
            .await;

        match outcome {
            Outcome::Done(response) | Outcome::Fail(response) => return response,
            Outcome::Rotate { terminal } => terminal_status = terminal,
        }
    }

    if terminal_status == 429 {
        error_response(429, "all claude subs are used up")
    } else {
        error_response(
            503,
            state
                .last_error()
                .unwrap_or_else(|| "no authorized claude subs are available".into()),
        )
    }
}

/// One attempt against one sub: refresh, send, retry a 401 once, classify.
async fn attempt(
    state: &Arc<ProxyState>,
    entry: &SubEntry,
    route: &str,
    forward: &Forward,
) -> Outcome {
    let span = tracing::Span::current();
    let id = entry.id;
    let mut sub = entry.sub.clone();

    if let Some(outcome) = ensure_fresh(state, id, &mut sub, false).await {
        return outcome;
    }

    let mut record = RequestRecord::new(
        state.clone(),
        id,
        entry.key().clone(),
        Provider::Claude,
        route,
    );
    let started = Instant::now();

    // Repeating the exchange re-bills the model call, so it happens once.
    let mut resent = false;
    loop {
        let sent_with = sub.credentials.tokens.access.clone();
        let mut response =
            match send_retrying(Provider::Claude, entry.key(), || send(&sub, forward)).await {
                Ok(response) => response,
                Err(e) => return transport_failed(state, &mut record, &span, &e),
            };

        let mut retried_auth = false;
        if response.status() == StatusCode::UNAUTHORIZED {
            drop(response);
            // A concurrent refresh may already have replaced the token; forcing another
            // would rotate the refresh token for nothing.
            if let Some(latest) = state.subs.get(id) {
                sub = latest.sub;
            }
            let force = sub.credentials.tokens.access == sent_with;
            if let Some(outcome) = ensure_fresh(state, id, &mut sub, force).await {
                return outcome;
            }
            retried_auth = true;
            response =
                match send_retrying(Provider::Claude, entry.key(), || send(&sub, forward)).await {
                    Ok(response) => response,
                    Err(e) => return transport_failed(state, &mut record, &span, &e),
                };
        }

        let status = response.status();
        span.record("status", status.as_u16());
        record.set_status(status.as_u16());

        // Every response carries the unified rate-limit headers, success or not.
        let observed = observe_rate_limits(state, entry, response.headers());

        if status.is_success() {
            state.note_success();
            span.record("duration_ms", started.elapsed().as_millis() as u64);
            if forward.client_stream {
                return Outcome::Done(stream_response(response, record));
            }
            match buffered_response(response, &mut record).await {
                Ok(response) => return Outcome::Done(response),
                // Not one byte has reached the client yet, so ask again.
                Err(e) if !resent => {
                    resent = true;
                    tracing::warn!(
                        provider = %Provider::Claude,
                        sub = %entry.key(),
                        error = %crate::error::chain(&e),
                        "upstream response died mid-body; asking the same sub again",
                    );
                    continue;
                }
                Err(e) => {
                    let message = format!("upstream response failed: {}", crate::error::chain(&e));
                    state.note_error(&message);
                    record.set_status(502);
                    span.record("status", 502);
                    return Outcome::Fail(error_response(502, message));
                }
            }
        }

        let upstream_headers = passthrough_headers(response.headers());
        let text = response.text().await.unwrap_or_default();
        let excerpt = body_excerpt(&text);
        span.record("duration_ms", started.elapsed().as_millis() as u64);

        let mut attempt = Attempt::new(status.as_u16(), &text);
        // The identity block was prepended into this body, so the flag's precondition holds.
        if request_scoped(status.as_u16(), &text) {
            attempt = attempt.request_scoped();
        }
        if retried_auth {
            attempt = attempt.after_auth_retry();
        }
        let class = attempt.classify();

        // The headers we just read are fresher and cheaper than a forced refetch.
        let usage = if class == FailureClass::UsageLimit {
            match observed {
                Some(usage) => Some(usage),
                None => usage_for_exhaustion(state, &sub).await,
            }
        } else {
            None
        };

        let disposition = state.router.on_failure(id, class, usage.as_ref());
        if class.quarantines()
            && let Some(until) = state.router.exhausted_until(id)
        {
            tracing::info_span!(
                "sub.exhausted",
                sub = %entry.key(),
                until = %until,
                cause = ?class,
            )
            .in_scope(|| tracing::info!("quarantined"));
        }

        return match disposition {
            Disposition::Rotate => {
                state.note_error(match class {
                    FailureClass::UsageLimit => {
                        format!("{} used up, failing over", entry.sub.label)
                    }
                    _ => format!("{} is unauthorized, failing over", entry.sub.label),
                });
                Outcome::Rotate {
                    terminal: if class == FailureClass::UsageLimit {
                        429
                    } else {
                        503
                    },
                }
            }
            Disposition::Fail { status } => {
                state.note_error(format!("upstream {status}: {excerpt}"));
                Outcome::Fail(error_response(status, excerpt))
            }
            // A first 401 is retried inline above, so `RetrySameSub` only arrives here if the
            // taxonomy grows a new case; passing the upstream response through is conservative.
            Disposition::PassThrough | Disposition::RetrySameSub => {
                state.note_error(format!("upstream {}", status.as_u16()));
                Outcome::Done((status, upstream_headers, text).into_response())
            }
        };
    }
}

/// The 502 for a request whose transport died before any response header.
fn transport_failed(
    state: &Arc<ProxyState>,
    record: &mut RequestRecord,
    span: &tracing::Span,
    error: &reqwest::Error,
) -> Outcome {
    let message = format!("upstream request failed: {}", crate::error::chain(error));
    state.note_error(&message);
    record.set_status(502);
    span.record("status", 502);
    Outcome::Fail(error_response(502, message))
}

/// Refresh if needed. `Some(outcome)` means the attempt is over.
async fn ensure_fresh(
    state: &Arc<ProxyState>,
    id: SubId,
    sub: &mut Sub,
    force: bool,
) -> Option<Outcome> {
    match state.tokens.ensure_fresh(sub, force).await {
        Ok(false) => None,
        Ok(true) => {
            state.persist_tokens(id, sub.credentials.tokens.clone());
            None
        }
        Err(e) => {
            let class = if e.permanent {
                FailureClass::RefreshPermanent
            } else {
                FailureClass::RefreshTransient
            };
            state.note_error(format!("token refresh failed: {e}"));
            match state.router.on_failure(id, class, None) {
                // Transient: 502 and NO quarantine — a flaky network must not disable an account.
                Disposition::Fail { status } => Some(Outcome::Fail(error_response(
                    status,
                    format!("token refresh failed: {e}"),
                ))),
                Disposition::Rotate => Some(Outcome::Rotate { terminal: 503 }),
                _ => Some(Outcome::Fail(error_response(
                    503,
                    format!("token refresh failed: {e}"),
                ))),
            }
        }
    }
}

/// One upstream request with a completely fresh header set: no client header survives, in
/// particular its `x-api-key` and `authorization`. The one exception is [`merged_beta`].
async fn send(sub: &Sub, forward: &Forward) -> reqwest::Result<reqwest::Response> {
    let mut authorization = reqwest::header::HeaderValue::from_str(&format!(
        "Bearer {}",
        sub.credentials.tokens.access
    ))
    .unwrap_or_else(|_| reqwest::header::HeaderValue::from_static("Bearer"));
    authorization.set_sensitive(true);

    let mut request = upstream(forward.method.clone(), &forward.url)
        .header(reqwest::header::AUTHORIZATION, authorization)
        .header("anthropic-beta", forward.beta.as_str())
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header("accept", forward.accept)
        .header("user-agent", ORIGINATOR);
    if let Some(body) = &forward.body {
        request = request
            .header("content-type", "application/json")
            .body(body.clone());
    }
    request.send().await
}

/// `Err` is a transport failure with nothing yet written to the client, so the caller may have
/// the whole exchange again.
async fn buffered_response(
    response: reqwest::Response,
    record: &mut RequestRecord,
) -> reqwest::Result<Response> {
    let status = response.status();
    let headers = passthrough_headers(response.headers());
    let body = response.bytes().await?;
    if let Ok(value) = serde_json::from_slice::<Value>(&body) {
        let (input, output) = token_counts(&value);
        record.set_tokens(input, output);
    }
    Ok((status, headers, body).into_response())
}

/// The upstream bytes unchanged; frames are only sniffed for the two events that carry counts.
/// [`RequestRecord`] moves into the body so the in-flight gauge is held for the whole exchange.
fn stream_response(response: reqwest::Response, record: RequestRecord) -> Response {
    let status = response.status();
    let headers = passthrough_headers(response.headers());

    struct StreamState {
        inner: std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<Bytes>> + Send>>,
        framer: SseFramer,
        record: RequestRecord,
        input: Option<u64>,
        output: Option<u64>,
        recorded: bool,
        finished: bool,
    }

    fn observe(state: &mut StreamState, frame: &str) {
        let Some(event) = sse::data_payload(frame)
            .and_then(|payload| serde_json::from_str::<Value>(&payload).ok())
        else {
            return;
        };
        if let Some(carrier) = stream_usage(&event) {
            let (input, output) = token_counts(carrier);
            // `message_delta` reports a running total, so the last one wins.
            state.input = input.or(state.input);
            state.output = output.or(state.output);
        }
        if event.get("type").and_then(Value::as_str) == Some("message_stop") {
            flush(state);
        }
    }

    /// Once, whichever comes first: `message_stop` or EOF.
    fn flush(state: &mut StreamState) {
        if state.recorded {
            return;
        }
        state.recorded = true;
        let (input, output) = (state.input, state.output);
        state.record.set_tokens(input, output);
    }

    let stream_state = StreamState {
        inner: Box::pin(response.bytes_stream()),
        framer: SseFramer::new(),
        record,
        input: None,
        output: None,
        recorded: false,
        finished: false,
    };

    let body = futures_util::stream::unfold(stream_state, |mut state| async move {
        if state.finished {
            return None;
        }
        match state.inner.next().await {
            Some(Ok(chunk)) => {
                for frame in state.framer.push(&chunk) {
                    observe(&mut state, &frame);
                }
                Some((Ok(chunk), state))
            }
            Some(Err(e)) => {
                state.finished = true;
                flush(&mut state);
                // Bytes already reached the client, but the row must not claim the 200 the
                // headers promised.
                state.record.set_status(502);
                tracing::warn!(
                    provider = %Provider::Claude,
                    error = %crate::error::chain(&e),
                    "upstream stream failed after the client had bytes",
                );
                Some((Err(e), state))
            }
            None => {
                state.finished = true;
                if let Some(tail) = state.framer.flush() {
                    observe(&mut state, &tail);
                }
                flush(&mut state);
                None
            }
        }
    });

    let status = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::OK);
    (status, headers, Body::from_stream(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn object(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            other => panic!("not an object: {other}"),
        }
    }

    #[test]
    fn the_identity_block_is_made_first_and_never_duplicated() {
        // Proxied Claude Code traffic must be a no-op passthrough.
        let original = json!({
            "system": [
                { "type": "text", "text": IDENTITY, "cache_control": { "type": "ephemeral" } },
                { "type": "text", "text": "caller" },
            ]
        });
        let mut body = object(original.clone());
        assert!(!ensure_identity_first(&mut body));
        assert_eq!(Value::Object(body), original);

        // The measured 429 case: right string, wrong position.
        let mut body = object(json!({
            "system": [
                { "type": "text", "text": "caller" },
                { "type": "text", "text": IDENTITY },
            ]
        }));
        assert!(ensure_identity_first(&mut body));
        assert_eq!(
            body["system"],
            json!([
                { "type": "text", "text": IDENTITY },
                { "type": "text", "text": "caller" },
            ])
        );

        let mut body = object(json!({ "system": [IDENTITY] }));
        assert!(!ensure_identity_first(&mut body));
        assert_eq!(body["system"], json!([IDENTITY]));

        for empty in [json!(""), Value::Null] {
            let mut body = object(json!({ "system": empty }));
            assert!(ensure_identity_first(&mut body));
            assert_eq!(
                body["system"],
                json!([{ "type": "text", "text": IDENTITY }])
            );
        }
    }

    #[test]
    fn token_counts_add_the_cache_fields_and_tolerate_a_bare_body() {
        let value = json!({
            "usage": {
                "input_tokens": 10,
                "cache_creation_input_tokens": 3,
                "cache_read_input_tokens": 5,
                "output_tokens": 7,
            }
        });
        assert_eq!(token_counts(&value), (Some(18), Some(7)));
        // A `count_tokens` body has no envelope and no output count.
        assert_eq!(
            token_counts(&json!({ "input_tokens": 42 })),
            (Some(42), None)
        );
        assert_eq!(token_counts(&json!({ "id": "msg_1" })), (None, None));
    }

    #[test]
    fn message_start_and_message_delta_are_the_two_usage_carriers() {
        let start = json!({
            "type": "message_start",
            "message": { "usage": { "input_tokens": 11, "output_tokens": 1 } },
        });
        assert_eq!(
            token_counts(stream_usage(&start).unwrap()),
            (Some(11), Some(1))
        );

        let delta = json!({ "type": "message_delta", "usage": { "output_tokens": 22 } });
        assert_eq!(
            token_counts(stream_usage(&delta).unwrap()),
            (None, Some(22))
        );

        let ping = json!({ "type": "ping" });
        assert!(stream_usage(&ping).is_none());
    }
}
