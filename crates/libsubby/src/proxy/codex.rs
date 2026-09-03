//! The Codex path: `POST /v1/responses` and `GET /v1/models`. The backend only
//! streams, rejects `store: true`, spells the system role `developer` and keeps
//! no conversation state, so every request is rewritten on the way out and
//! `previous_response_id` is emulated from [`crate::store::transcripts`].

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use tracing::Instrument;

use crate::balance::{self, Attempt, Disposition, FailureClass, Hints};
use crate::model::{Provider, Sub, SubId, SubKey};
use crate::store::transcripts::{Chain, TranscriptStore, Turn};

use super::sse::{self, Aggregated, Aggregator, SseFramer};
use super::transcript;
use super::{
    ORIGINATOR, ProxyState, Rejection, RequestRecord, SubEntry, account_id_header, body_excerpt,
    error_response, is_json_media_type, new_request_id, passthrough_headers, read_body, reject,
    send_retrying, upstream, usage_for_exhaustion,
};

/// The backend gates its catalog on the Codex CLI version; an unknown one gets a partial list.
pub const CLIENT_VERSION: &str = "0.147.0";

pub const MODEL_CACHE_TTL: Duration = Duration::from_secs(5 * 60);

/// The catalog is a menu nicety, not the request path, so it gets its own short timeout.
pub const MODEL_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[must_use]
pub fn responses_url(base: &str) -> String {
    format!("{}/codex/responses", base.trim_end_matches('/'))
}

#[must_use]
pub fn models_url(base: &str) -> String {
    format!(
        "{}/codex/models?client_version={CLIENT_VERSION}",
        base.trim_end_matches('/')
    )
}

/// Keys the Codex backend does not accept. `prompt_cache_key` is also routing input, so
/// [`prepare`] has to read it before this deletes it.
pub const DROPPED_KEYS: [&str; 4] = [
    "prompt_cache_key",
    "prompt_cache_retention",
    "prompt_cache_options",
    "max_output_tokens",
];

/// Rewrite a request body into what the Codex backend accepts. `store` and `stream` are the
/// caller's job.
pub fn normalize_for_codex_backend(body: &mut Map<String, Value>) {
    for key in DROPPED_KEYS {
        body.remove(key);
    }
    let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    for item in input {
        let Some(message) = item.as_object_mut() else {
            continue;
        };
        if message.get("role").and_then(Value::as_str) == Some("system") {
            message.insert("role".into(), Value::String("developer".into()));
        }
        message.remove("prompt_cache_breakpoint");
        let Some(parts) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for part in parts {
            if let Some(part) = part.as_object_mut() {
                part.remove("prompt_cache_breakpoint");
            }
        }
    }
}

struct Prepared {
    body: Vec<u8>,
    /// Upstream is always streamed; this is what the *client* asked for.
    client_stream: bool,
    /// Already deleted from `body`; restored on the way out and recorded as this turn's parent.
    previous_response_id: Option<String>,
    /// Read before normalisation deleted it. `None` when absent, empty or not a string.
    key: Option<String>,
    hints: Hints,
    /// Only the items the client sent *this* turn, so storing a chain costs O(n) bytes.
    delta: Vec<Value>,
}

/// Both store lookups in one blocking hop: a chained request usually carries a key too.
#[derive(Default)]
struct Resolved {
    /// `None` when the request named no parent.
    chain: Option<crate::error::Result<Option<Chain>>>,
    /// `None` when the request carried no key.
    placement: Option<crate::error::Result<Option<SubKey>>>,
}

pub async fn responses(State(state): State<Arc<ProxyState>>, request: Request) -> Response {
    // A browser-safelisted content type must not be able to drive a proxy holding OAuth tokens.
    if !is_json_media_type(request.headers()) {
        return error_response(415, "content-type must be application/json");
    }
    let route = request.uri().path().to_owned();
    let (_, body) = request.into_parts();
    let raw = match read_body(body).await {
        Ok(raw) => raw,
        Err(rejection) => return *rejection,
    };

    let prepared = match prepare(&state, &route, &raw).await {
        Ok(prepared) => prepared,
        Err(rejection) => return *rejection,
    };
    forward_with_failover(state, route, prepared).await
}

async fn prepare(state: &Arc<ProxyState>, route: &str, raw: &[u8]) -> Result<Prepared, Rejection> {
    let value: Value = serde_json::from_slice(raw).map_err(|_| reject(400, "invalid JSON body"))?;
    let Value::Object(mut body) = value else {
        return Err(reject(400, "JSON body must be an object"));
    };

    // Captured before the rewrites below splice the chain into `input` and drop the cache key.
    let previous_response_id = transcript::previous_response_id(&body);
    let key = body
        .get("prompt_cache_key")
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty())
        .map(str::to_owned);
    let delta = transcript::response_input_items(body.get("input").unwrap_or(&Value::Null));

    let mut hints = Hints::default();
    if previous_response_id.is_some() || key.is_some() {
        let store = state.transcripts.clone();
        let head = previous_response_id.clone();
        let placed = key.clone();
        let resolved = tokio::task::spawn_blocking(move || Resolved {
            chain: head.map(|head| store.chain(&head)),
            placement: placed.map(|key| store.placement(&key)),
        })
        .await;
        let resolved = match resolved {
            Ok(resolved) => resolved,
            Err(e) => {
                tracing::warn!(error = %e, %route, "reading the transcript store panicked");
                if previous_response_id.is_some() {
                    return Err(reject(500, "transcript store failed"));
                }
                Resolved::default()
            }
        };

        if let Some((previous, chain)) = previous_response_id.as_ref().zip(resolved.chain) {
            match chain {
                Ok(Some(chain)) => {
                    hints.chain = state.subs.id_of(&chain.sub);
                    transcript::splice(&mut body, &chain);
                }
                Ok(None) => {
                    tracing::warn!(id = %previous, %route, "shedding a request chained off an id no chain can be assembled for");
                    return Err(reject(
                        transcript::UnknownPreviousResponse::HTTP_STATUS,
                        transcript::UnknownPreviousResponse {
                            id: previous.clone(),
                        },
                    ));
                }
                Err(e) => {
                    tracing::warn!(error = %e, %route, "the transcript store could not resolve a chain");
                    return Err(reject(500, format!("transcript store failed: {e}")));
                }
            }
        }

        // A placement is cache warmth, never correctness: an unreadable one costs only the
        // account preference, where an unresolvable chain is a 400 or a 500.
        match resolved.placement {
            Some(Ok(placed)) => hints.key = placed.and_then(|sub| state.subs.id_of(&sub)),
            Some(Err(e)) => {
                tracing::warn!(error = %e, %route, "could not read where a prompt_cache_key was placed");
            }
            None => {}
        }
    }

    body.insert("store".into(), Value::Bool(false));
    normalize_for_codex_backend(&mut body);

    // The backend only serves SSE; the client's `stream` decides only whether we aggregate.
    let client_stream = body.get("stream") == Some(&Value::Bool(true));
    body.insert("stream".into(), Value::Bool(true));

    let serialised = serde_json::to_vec(&body)
        .map_err(|e| reject(500, format!("could not re-encode the request body: {e}")))?;

    Ok(Prepared {
        body: serialised,
        client_stream,
        previous_response_id,
        key,
        hints,
        delta,
    })
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
    prepared: Prepared,
) -> Response {
    let hints = prepared.hints;

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
            .statuses(Provider::Codex, &state.metrics, Some(&state.usage));
        let scorer = state.scorer(Provider::Codex);
        let select_span = tracing::info_span!(
            "balance.select",
            provider = "codex",
            pool = pool.as_ref().map(|p| p.name.as_str()).unwrap_or("-"),
            strategy = %state.router.settings().strategy,
            candidates = tracing::field::Empty,
            chosen = tracing::field::Empty,
            reason = tracing::field::Empty,
            usage_round = tracing::field::Empty,
        );
        let selection = state
            .router
            .select_in(Provider::Codex, &statuses, hints, pool.as_ref(), &scorer)
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

        // Placed before the upstream call, so a burst of first requests on a new key converges.
        if let Some(key) = &prepared.key
            && hints.key != Some(selection.sub)
        {
            place(&state, key, entry.key()).await;
        }

        let span = tracing::info_span!(
            "proxy.request",
            provider = "codex",
            route = %route,
            attempt = attempt_no,
            sub = %entry.key(),
            reason = tracing::field::debug(selection.reason),
            status = tracing::field::Empty,
            duration_ms = tracing::field::Empty,
        );
        let outcome = attempt(&state, &entry, &route, &prepared)
            .instrument(span)
            .await;

        match outcome {
            Outcome::Done(response) => return response,
            Outcome::Fail(response) => return response,
            Outcome::Rotate { terminal } => {
                terminal_status = terminal;
            }
        }
    }

    if terminal_status == 429 {
        error_response(429, "all codex subs are used up")
    } else {
        error_response(
            503,
            state
                .last_error()
                .unwrap_or_else(|| "no authorized codex subs are available".into()),
        )
    }
}

/// One attempt against one sub: refresh, send, retry a 401 once, classify.
async fn attempt(
    state: &Arc<ProxyState>,
    entry: &SubEntry,
    route: &str,
    prepared: &Prepared,
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
        Provider::Codex,
        route,
    );
    let started = Instant::now();

    // Repeating the exchange re-bills the model call, so it happens once.
    let mut resent = false;
    loop {
        let sent_with = sub.credentials.tokens.access.clone();
        let mut response = match send_retrying(Provider::Codex, entry.key(), || {
            send(state, &sub, &prepared.body)
        })
        .await
        {
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
            response = match send_retrying(Provider::Codex, entry.key(), || {
                send(state, &sub, &prepared.body)
            })
            .await
            {
                Ok(response) => response,
                Err(e) => return transport_failed(state, &mut record, &span, &e),
            };
        }

        let status = response.status();
        span.record("status", status.as_u16());
        record.set_status(status.as_u16());

        if status.is_success() {
            state.note_success();
            span.record("duration_ms", started.elapsed().as_millis() as u64);
            if prepared.client_stream {
                return Outcome::Done(stream_response(
                    state,
                    response,
                    prepared,
                    entry.key().clone(),
                    record,
                ));
            }
            match aggregate(state, response, prepared, &mut record, entry.key()).await {
                Ok(response) => return Outcome::Done(response),
                // Not one byte has reached the client yet, so ask again.
                Err(e) if !resent => {
                    resent = true;
                    tracing::warn!(
                        provider = %Provider::Codex,
                        sub = %entry.key(),
                        error = %crate::error::chain(&e),
                        "upstream response died mid-body; asking the same sub again",
                    );
                    continue;
                }
                Err(e) => {
                    let message = format!(
                        "upstream response stream failed: {}",
                        crate::error::chain(&e)
                    );
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
        if request_scoped(status.as_u16(), &text) {
            attempt = attempt.request_scoped();
        }
        if retried_auth {
            attempt = attempt.after_auth_retry();
        }
        let class = attempt.classify();

        let usage = if class == FailureClass::UsageLimit {
            usage_for_exhaustion(state, &sub).await
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
                state.note_error(format!("upstream {}: {excerpt}", status));
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

/// Whether a failure is about the bytes we sent rather than the credential we sent them with.
/// A 401 and a usage-limit 429 are the only two the upstream has exonerated the body of;
/// everything else goes back to the client rather than burning one account per candidate.
#[must_use]
pub fn request_scoped(status: u16, body: &str) -> bool {
    status != 401 && !balance::is_usage_limit_error(status, body)
}

/// One upstream request with a completely fresh header set: merging the client's would leak
/// its `authorization`, `user-agent` and cookies into somebody else's account.
async fn send(state: &ProxyState, sub: &Sub, body: &[u8]) -> reqwest::Result<reqwest::Response> {
    let request_id = new_request_id();
    let mut authorization = reqwest::header::HeaderValue::from_str(&format!(
        "Bearer {}",
        sub.credentials.tokens.access
    ))
    .unwrap_or_else(|_| reqwest::header::HeaderValue::from_static("Bearer"));
    authorization.set_sensitive(true);

    upstream(
        reqwest::Method::POST,
        &responses_url(state.base(Provider::Codex)),
    )
    .header(reqwest::header::AUTHORIZATION, authorization)
    .header("chatgpt-account-id", account_id_header(sub))
    .header("content-type", "application/json")
    .header("accept", "text/event-stream")
    .header("openai-beta", "responses=experimental")
    .header("originator", ORIGINATOR)
    .header("user-agent", ORIGINATOR)
    .header("session-id", &request_id)
    .header("x-client-request-id", &request_id)
    .body(body.to_vec())
    .send()
    .await
}

/// `Err` is a transport failure with nothing yet written to the client, so the caller may have
/// the whole exchange again; a missing terminal event comes back as an `Ok` 502 instead.
async fn aggregate(
    state: &Arc<ProxyState>,
    response: reqwest::Response,
    prepared: &Prepared,
    record: &mut RequestRecord,
    sub: &SubKey,
) -> reqwest::Result<Response> {
    let mut stream = response.bytes_stream();
    let mut framer = SseFramer::new();
    let mut aggregator = Aggregator::new();

    'read: while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(e) => return Err(e),
        };
        for frame in framer.push(&chunk) {
            aggregator.consume(&frame);
            // Bytes may still be buffered and the connection may never close.
            if aggregator.is_done() {
                break 'read;
            }
        }
    }
    if !aggregator.is_done()
        && let Some(tail) = framer.flush()
    {
        aggregator.consume(&tail);
    }
    // Dropping cancels the upstream reader rather than draining to an EOF that may never come.
    drop(stream);

    let mut terminal = match aggregator.finish() {
        Aggregated::Terminal(value) => value,
        other => {
            let message = other
                .error_message()
                .unwrap_or_else(|| Aggregated::NO_TERMINAL_MESSAGE.to_owned());
            state.note_error(&message);
            record.set_status(502);
            return Ok(error_response(502, message));
        }
    };

    if let Some(previous) = &prepared.previous_response_id
        && let Some(object) = terminal.as_object_mut()
    {
        object.insert(
            "previous_response_id".into(),
            Value::String(previous.clone()),
        );
    }

    if let Some(turn) = turn_of(
        &terminal,
        &prepared.previous_response_id,
        sub,
        &prepared.delta,
    ) {
        remember(state.transcripts.clone(), turn).await;
    }

    let (input_tokens, output_tokens) = usage_counts(&terminal);
    record.set_tokens(input_tokens, output_tokens);
    Ok(axum::Json(terminal).into_response())
}

/// `None` when the response has no `id` or `output` and so is nothing a later request can name.
fn turn_of(
    terminal: &Value,
    parent: &Option<String>,
    sub: &SubKey,
    delta: &[Value],
) -> Option<Turn> {
    Some(Turn {
        id: terminal.get("id").and_then(Value::as_str)?.to_owned(),
        parent: parent.clone(),
        sub: sub.clone(),
        input: delta.to_vec(),
        output: terminal.get("output").and_then(Value::as_array)?.clone(),
    })
}

/// Awaited before the terminal frame reaches the client: a client handed the id first could
/// chain off it before the row lands.
async fn remember(store: Arc<TranscriptStore>, turn: Turn) {
    let id = turn.id.clone();
    match tokio::task::spawn_blocking(move || store.remember(turn)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::warn!(error = %e, %id, "could not remember a turn; chaining off it will 400");
        }
        Err(e) => tracing::warn!(error = %e, %id, "remembering a turn panicked"),
    }
}

/// Which account a `prompt_cache_key` was sent to. A failure only costs the next request its
/// cache warmth, so it is a warning and nothing more.
async fn place(state: &Arc<ProxyState>, key: &str, sub: &SubKey) {
    let store = state.transcripts.clone();
    let (key, sub) = (key.to_owned(), sub.clone());
    match tokio::task::spawn_blocking(move || store.place(&key, &sub)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "could not place a prompt_cache_key"),
        Err(e) => tracing::warn!(error = %e, "placing a prompt_cache_key panicked"),
    }
}

fn usage_counts(response: &Value) -> (Option<u64>, Option<u64>) {
    let Some(usage) = response.get("usage") else {
        return (None, None);
    };
    (
        usage.get("input_tokens").and_then(Value::as_u64),
        usage.get("output_tokens").and_then(Value::as_u64),
    )
}

/// Stream to the client while folding the frames into a turn: `previous_response_id` is put
/// back into them (upstream never heard of it, the chain went into `input` instead), and the
/// turn is stored before the terminal chunk is yielded, since that chunk is what tells the
/// client the id it may chain off. With nothing to rewrite the bytes are forwarded unchanged.
fn stream_response(
    state: &Arc<ProxyState>,
    response: reqwest::Response,
    prepared: &Prepared,
    sub: SubKey,
    record: RequestRecord,
) -> Response {
    let status = response.status();
    let headers = passthrough_headers(response.headers());

    struct StreamState {
        inner: std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<Bytes>> + Send>>,
        framer: SseFramer,
        rewrite: Option<String>,
        record: RequestRecord,
        finished: bool,
        store: Arc<TranscriptStore>,
        /// Taken once the turn is stored, so the rest of the stream costs nothing.
        aggregator: Option<Aggregator>,
        parent: Option<String>,
        sub: SubKey,
        delta: Vec<Value>,
    }

    fn observe(state: &mut StreamState, frame: &str) {
        let (input, output) = match sse::data_payload(frame)
            .and_then(|payload| serde_json::from_str::<Value>(&payload).ok())
            .as_ref()
            .and_then(|event| event.get("response"))
            .map(usage_counts)
        {
            Some(counts) => counts,
            None => return,
        };
        state.record.set_tokens(input, output);
    }

    async fn absorb(state: &mut StreamState, frames: &[String]) {
        for frame in frames {
            observe(state, frame);
            let Some(aggregator) = state.aggregator.as_mut() else {
                continue;
            };
            aggregator.consume(frame);
            if !aggregator.is_done() {
                continue;
            }
            let Some(Aggregated::Terminal(terminal)) =
                state.aggregator.take().map(Aggregator::finish)
            else {
                continue;
            };
            if let Some(turn) = turn_of(&terminal, &state.parent, &state.sub, &state.delta) {
                remember(state.store.clone(), turn).await;
            }
        }
    }

    let state = StreamState {
        inner: Box::pin(response.bytes_stream()),
        framer: SseFramer::new(),
        rewrite: prepared.previous_response_id.clone(),
        record,
        finished: false,
        store: state.transcripts.clone(),
        aggregator: Some(Aggregator::new()),
        parent: prepared.previous_response_id.clone(),
        sub,
        delta: prepared.delta.clone(),
    };

    let body = futures_util::stream::unfold(state, |mut state| async move {
        if state.finished {
            return None;
        }
        loop {
            match state.inner.next().await {
                Some(Ok(chunk)) => {
                    let frames = state.framer.push(&chunk);
                    absorb(&mut state, &frames).await;
                    match state.rewrite.clone() {
                        None => return Some((Ok(chunk), state)),
                        Some(previous) => {
                            if frames.is_empty() {
                                continue;
                            }
                            let mut out = String::new();
                            for frame in &frames {
                                out.push_str(&sse::rewrite_previous_response_id(frame, &previous));
                                out.push_str("\n\n");
                            }
                            return Some((Ok(Bytes::from(out)), state));
                        }
                    }
                }
                Some(Err(e)) => {
                    state.finished = true;
                    // Bytes already reached the client, but the row must not claim the 200 the
                    // headers promised.
                    state.record.set_status(502);
                    tracing::warn!(
                        provider = %Provider::Codex,
                        error = %crate::error::chain(&e),
                        "upstream stream failed after the client had bytes",
                    );
                    return Some((Err(e), state));
                }
                None => {
                    state.finished = true;
                    let tail = state.framer.flush()?;
                    absorb(&mut state, std::slice::from_ref(&tail)).await;
                    if let Some(previous) = state.rewrite.clone() {
                        let out = sse::rewrite_previous_response_id(&tail, &previous);
                        return Some((Ok(Bytes::from(out)), state));
                    }
                    return None;
                }
            }
        }
    });

    let status = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::OK);
    (status, headers, Body::from_stream(body)).into_response()
}

/// One fetched catalog in both envelopes at once: an OpenAI-compatible client reads
/// `{"object":"list","data":[…]}` and `codex` decodes the upstream `{"models":[…]}` off the
/// same URL, and extra keys are ignored, so one body satisfies both. `upstream` stays exactly
/// as it arrived because `codex`'s `ModelInfo` is wide and `{"slug": id}` stubs fail to decode.
#[derive(Debug, Default, PartialEq)]
pub struct Catalog {
    pub upstream: Vec<Value>,
    /// The slugs, deduplicated, in upstream order.
    pub ids: Vec<String>,
}

#[derive(Debug, Default)]
pub struct ModelCatalog {
    fetched_at: Option<Instant>,
    catalog: Option<Arc<Catalog>>,
}

impl ModelCatalog {
    fn fresh(&self) -> Option<Arc<Catalog>> {
        self.fetched_at
            .filter(|at| at.elapsed() < MODEL_CACHE_TTL)
            .and(self.catalog.clone())
    }

    /// The last catalog fetched, however old: a stale list beats a 503.
    fn stale(&self) -> Option<Arc<Catalog>> {
        self.catalog.clone()
    }

    fn store(&mut self, catalog: Arc<Catalog>) {
        self.fetched_at = Some(Instant::now());
        self.catalog = Some(catalog);
    }
}

fn model_object(id: &str) -> Value {
    json!({ "id": id, "object": "model", "created": 0, "owned_by": "openai" })
}

#[derive(Debug, serde::Serialize)]
struct ModelsBody<'a> {
    object: &'static str,
    data: Vec<Value>,
    /// What `codex` decodes, verbatim. See [`Catalog`].
    models: &'a [Value],
}

/// One model in both shapes. The OpenAI keys win a collision, and an id upstream did not
/// describe still yields a bare, valid model object.
fn model_entry(catalog: &Catalog, id: &str) -> Value {
    let mut entry = catalog
        .upstream
        .iter()
        .find(|entry| entry.get("slug").and_then(Value::as_str) == Some(id))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Value::Object(openai) = model_object(id) {
        entry.extend(openai);
    }
    Value::Object(entry)
}

pub async fn models(State(state): State<Arc<ProxyState>>) -> Response {
    match catalog(&state).await {
        Ok(catalog) => axum::Json(ModelsBody {
            object: "list",
            data: catalog.ids.iter().map(|id| model_object(id)).collect(),
            models: &catalog.upstream,
        })
        .into_response(),
        Err((status, message)) => error_response(status, message),
    }
}

pub async fn model(State(state): State<Arc<ProxyState>>, request: Request) -> Response {
    let Some(model) = crate::proxy::model_from_path(request.uri().path()).map(str::to_owned) else {
        return error_response(404, "no model named in the path");
    };
    match catalog(&state).await {
        Ok(catalog) if catalog.ids.iter().any(|id| id == &model) => {
            axum::Json(model_entry(&catalog, &model)).into_response()
        }
        Ok(_) => error_response(404, format!("model '{model}' not found")),
        Err((status, message)) => error_response(status, message),
    }
}

async fn catalog(state: &Arc<ProxyState>) -> Result<Arc<Catalog>, (u16, String)> {
    if let Some(fresh) = state.codex_models().fresh() {
        return Ok(fresh);
    }
    let cached = state.codex_models().stale();

    // The current sub first: its catalog matches the account actually serving requests.
    let all = state.subs.of_provider(Provider::Codex);
    let current = state.router.current(Provider::Codex);
    let mut ordered: Vec<SubEntry> = Vec::with_capacity(all.len());
    if let Some(entry) = current.and_then(|id| all.iter().find(|e| e.id == id)) {
        ordered.push(entry.clone());
    }
    ordered.extend(
        all.into_iter()
            .filter(|e| Some(e.id) != current && e.enabled && !e.needs_login),
    );

    if ordered.is_empty() {
        return cached.ok_or((503, "no codex subs are configured in subbier".to_owned()));
    }

    let mut failure = (502u16, "models request failed".to_owned());
    for entry in ordered {
        match fetch_catalog(state, &entry).await {
            CatalogAttempt::Fetched(catalog) => {
                state.codex_models().store(Arc::clone(&catalog));
                return Ok(catalog);
            }
            CatalogAttempt::Next(next) => failure = next,
            CatalogAttempt::Stop(stop) => {
                failure = stop;
                break;
            }
        }
    }
    cached.ok_or(failure)
}

enum CatalogAttempt {
    Fetched(Arc<Catalog>),
    /// This sub cannot serve the catalog; try another.
    Next((u16, String)),
    /// The upstream itself is unhappy; another account will not help.
    Stop((u16, String)),
}

async fn fetch_catalog(state: &Arc<ProxyState>, entry: &SubEntry) -> CatalogAttempt {
    let id = entry.id;
    let mut sub = entry.sub.clone();

    for forced in [false, true] {
        if forced {
            // Only reached after a 401.
            if let Some(latest) = state.subs.get(id) {
                sub = latest.sub;
            }
        }
        match state.tokens.ensure_fresh(&mut sub, forced).await {
            Ok(true) => state.persist_tokens(id, sub.credentials.tokens.clone()),
            Ok(false) => {}
            Err(e) => {
                if e.permanent {
                    state.router.exhaust(id, None);
                    return CatalogAttempt::Next((503, format!("token refresh failed: {e}")));
                }
                return CatalogAttempt::Next((502, format!("token refresh failed: {e}")));
            }
        }

        let mut authorization = reqwest::header::HeaderValue::from_str(&format!(
            "Bearer {}",
            sub.credentials.tokens.access
        ))
        .unwrap_or_else(|_| reqwest::header::HeaderValue::from_static("Bearer"));
        authorization.set_sensitive(true);

        let response = crate::http::client()
            .get(models_url(state.base(Provider::Codex)))
            .timeout(MODEL_REQUEST_TIMEOUT)
            .header(reqwest::header::AUTHORIZATION, authorization)
            .header("chatgpt-account-id", account_id_header(&sub))
            .header("originator", ORIGINATOR)
            .header("user-agent", ORIGINATOR)
            .send()
            .await;

        let response = match response {
            Ok(response) => response,
            Err(e) => return CatalogAttempt::Next((502, format!("models request failed: {e}"))),
        };

        if response.status() == StatusCode::UNAUTHORIZED && !forced {
            continue;
        }
        if response.status() == StatusCode::UNAUTHORIZED {
            state.router.exhaust(id, None);
            return CatalogAttempt::Next((503, format!("{} is unauthorized", entry.sub.label)));
        }
        if response.status() == StatusCode::FORBIDDEN {
            state.router.exhaust(id, None);
            return CatalogAttempt::Next((
                503,
                format!("{} cannot access the models catalog", entry.sub.label),
            ));
        }
        if !response.status().is_success() {
            let status = response.status().as_u16();
            return CatalogAttempt::Stop((status, format!("models upstream returned {status}")));
        }
        let body = match response.text().await {
            Ok(body) => body,
            Err(e) => return CatalogAttempt::Next((502, format!("models request failed: {e}"))),
        };
        return match parse_catalog(&body) {
            Some(catalog) => CatalogAttempt::Fetched(Arc::new(catalog)),
            None => {
                CatalogAttempt::Stop((502, "models upstream returned an invalid catalog".into()))
            }
        };
    }
    CatalogAttempt::Next((502, "models request failed".to_owned()))
}

/// An entry with no usable slug still rides along in [`Catalog::upstream`]; only
/// [`Catalog::ids`] has to be a clean routing list.
#[must_use]
pub fn parse_catalog(body: &str) -> Option<Catalog> {
    let value: Value = serde_json::from_str(body).ok()?;
    let Value::Array(upstream) = value.get("models")?.clone() else {
        return None;
    };
    let mut ids: Vec<String> = Vec::with_capacity(upstream.len());
    for model in &upstream {
        let Some(slug) = model.get("slug").and_then(Value::as_str) else {
            continue;
        };
        if !slug.is_empty() && !ids.iter().any(|seen| seen == slug) {
            ids.push(slug.to_owned());
        }
    }
    Some(Catalog { upstream, ids })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(json: &str) -> Map<String, Value> {
        match serde_json::from_str(json).unwrap() {
            Value::Object(map) => map,
            other => panic!("not an object: {other}"),
        }
    }

    #[test]
    fn normalisation_rewrites_the_system_role_and_deletes_the_cache_keys() {
        let mut body = object(
            r#"{
              "model": "gpt-5.5",
              "prompt_cache_key": "k",
              "prompt_cache_retention": "24h",
              "prompt_cache_options": {"mode": "explicit"},
              "max_output_tokens": 64,
              "input": [
                {"role":"system","type":"message",
                 "prompt_cache_breakpoint":{"mode":"explicit"},
                 "content":[{"type":"input_text","text":"sys",
                             "prompt_cache_breakpoint":{"mode":"explicit"}}]},
                {"role":"user","content":[{"type":"input_text","text":"hi"}]}
              ]
            }"#,
        );
        normalize_for_codex_backend(&mut body);

        for key in DROPPED_KEYS {
            assert!(!body.contains_key(key), "{key} survived");
        }
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["role"], "developer");
        assert!(input[0].get("prompt_cache_breakpoint").is_none());
        assert!(
            input[0]["content"][0]
                .get("prompt_cache_breakpoint")
                .is_none(),
            "the breakpoint on the content PART must go too"
        );
        assert_eq!(input[1]["role"], "user");
        assert_eq!(body["model"], "gpt-5.5");

        let mut body = object(r#"{"input":"hi","max_output_tokens":8}"#);
        normalize_for_codex_backend(&mut body);
        assert_eq!(body["input"], "hi");
        assert!(!body.contains_key("max_output_tokens"));

        let mut body = object(r#"{"input":[1,"two",null]}"#);
        normalize_for_codex_backend(&mut body);
        assert_eq!(body["input"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn a_failure_is_request_scoped_unless_the_upstream_exonerates_the_body() {
        assert!(!request_scoped(401, "unauthorized"));
        assert!(!request_scoped(
            429,
            r#"{"error":{"message":"Monthly usage limit reached (GoUsageLimitError)"}}"#
        ));
        // A 429 that does not name a usage limit could be our own rewrite.
        assert!(request_scoped(
            429,
            r#"{"error":{"type":"rate_limit_error","message":"Error"}}"#
        ));
        assert!(request_scoped(400, "Stream must be set to true"));
        assert!(request_scoped(422, "unsupported parameter"));
    }

    #[test]
    fn the_catalog_parser_takes_slugs_in_order_and_deduplicates() {
        let catalog = parse_catalog(
            r#"{"models":[{"slug":"gpt-5.6-sol"},{"slug":"gpt-dynamic"},
                          {"slug":"gpt-5.6-sol"},{"slug":""},{"no":"slug"}]}"#,
        )
        .unwrap();
        assert_eq!(catalog.ids, vec!["gpt-5.6-sol", "gpt-dynamic"]);
        // The ids are cleaned; the upstream array is not touched at all.
        assert_eq!(catalog.upstream.len(), 5);
        assert_eq!(catalog.upstream[4], json!({"no": "slug"}));
        assert!(parse_catalog("not json").is_none());
        assert!(parse_catalog(r#"{"models":"nope"}"#).is_none());
        assert!(parse_catalog(r#"{}"#).is_none());
        assert_eq!(parse_catalog(r#"{"models":[]}"#).unwrap().ids.len(), 0);

        // `codex` decodes these fields, so none may be dropped on the way through.
        let catalog = parse_catalog(
            r#"{"models":[{"slug":"gpt-5.6-sol","display_name":"GPT-5.6 Sol",
                          "supported_reasoning_levels":[{"effort":"low"}],
                          "base_instructions":"You are Codex."}]}"#,
        )
        .unwrap();
        assert_eq!(catalog.ids, vec!["gpt-5.6-sol"]);
        assert_eq!(catalog.upstream[0]["display_name"], "GPT-5.6 Sol");
        assert_eq!(catalog.upstream[0]["base_instructions"], "You are Codex.");
        assert_eq!(
            catalog.upstream[0]["supported_reasoning_levels"][0]["effort"],
            "low"
        );
    }

    #[test]
    fn a_model_entry_lays_the_openai_keys_over_the_upstream_one() {
        let catalog =
            parse_catalog(r#"{"models":[{"slug":"gpt-dynamic","display_name":"Dynamic"}]}"#)
                .unwrap();
        let entry = model_entry(&catalog, "gpt-dynamic");
        assert_eq!(entry["id"], "gpt-dynamic");
        assert_eq!(entry["object"], "model");
        assert_eq!(entry["created"], 0);
        assert_eq!(entry["owned_by"], "openai");
        assert_eq!(entry["slug"], "gpt-dynamic");
        assert_eq!(entry["display_name"], "Dynamic");
        // An id upstream never described still yields a valid model object.
        assert_eq!(
            model_entry(&catalog, "gpt-5.4"),
            json!({"id":"gpt-5.4","object":"model","created":0,"owned_by":"openai"})
        );
    }

    #[test]
    fn the_catalog_cache_expires_but_stays_available_stale() {
        let mut cache = ModelCatalog::default();
        assert!(cache.fresh().is_none());
        assert!(cache.stale().is_none());
        cache.store(Arc::new(
            parse_catalog(r#"{"models":[{"slug":"a"}]}"#).unwrap(),
        ));
        assert_eq!(cache.fresh().unwrap().ids, vec!["a".to_owned()]);
        assert_eq!(cache.stale().unwrap().ids, vec!["a".to_owned()]);
        cache.fetched_at = Some(Instant::now() - MODEL_CACHE_TTL - Duration::from_secs(1));
        assert!(cache.fresh().is_none(), "expired");
        assert_eq!(
            cache.stale().unwrap().ids,
            vec!["a".to_owned()],
            "a stale catalog beats a 503"
        );
        // The stale copy keeps the upstream document `codex` decodes.
        assert_eq!(cache.stale().unwrap().upstream, vec![json!({"slug": "a"})]);
    }
}
