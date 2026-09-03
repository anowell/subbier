//! The Claude (Anthropic Messages) proxy path against a local fake upstream.
//! The account letter keys the fake: A is 0% and always answers a genuine
//! usage-limit 429, B 40%, C 10%, F 0% and always 401, G 10%, H 5% with
//! `allowed` headers, R 5% with `rejected`. `model` picks per-request behaviour.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use jiff::{SignedDuration, Timestamp};
use libsubby::auth::{TokenManager, TokenUrls};
use libsubby::balance::{Router, RouterSettings};
use libsubby::model::{
    CredentialSource, Credentials, Provider, StrategyKind, Sub, SubId, SubKey, Tokens,
};
use libsubby::proxy::claude::IDENTITY;
use libsubby::proxy::{ProxyHandle, ProxyState, SubEntry, serve};
use libsubby::store::db::Db;
use libsubby::usage::Bases;
use serde_json::{Value, json};

#[derive(Default)]
struct FakeState {
    /// The account letter behind each `/v1/messages` call, in order.
    hits: Mutex<Vec<String>>,
    count_hits: Mutex<Vec<String>>,
    last_body: Mutex<Option<Value>>,
    last_headers: Mutex<Vec<(String, String)>>,
    refresh_hits: AtomicUsize,
    anthropic_model_hits: AtomicUsize,
    codex_model_hits: AtomicUsize,
}

impl FakeState {
    fn hits(&self) -> Vec<String> {
        self.hits.lock().unwrap().clone()
    }

    fn last_body(&self) -> Value {
        self.last_body
            .lock()
            .unwrap()
            .clone()
            .expect("the upstream was never called")
    }

    fn last_header(&self, name: &str) -> Option<String> {
        self.last_headers
            .lock()
            .unwrap()
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    }
}

/// `Bearer tok-F-refreshed` -> `"F"`: Anthropic sends no account header, so the
/// bearer is the only thing to key on.
fn letter_of(authorization: &str) -> String {
    authorization
        .trim_start_matches("Bearer ")
        .trim_start_matches("tok-")
        .split('-')
        .next()
        .unwrap_or_default()
        .to_owned()
}

fn used_percent(letter: &str) -> u32 {
    match letter {
        "B" => 40,
        "C" | "G" => 10,
        "H" | "R" => 5,
        _ => 0,
    }
}

fn iso_in_an_hour() -> String {
    (Timestamp::now() + SignedDuration::from_hours(1)).to_string()
}

fn form_value(body: &str, key: &str) -> Option<String> {
    body.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| v.replace('+', " "))
    })
}

/// The Anthropic SSE shape: `event:` plus `data:` per frame.
fn message_stream() -> String {
    [
        json!({ "type": "message_start",
                "message": { "id": "msg_1", "type": "message", "role": "assistant",
                             "usage": { "input_tokens": 11, "output_tokens": 0 } } }),
        json!({ "type": "content_block_delta", "index": 0,
                "delta": { "type": "text_delta", "text": "hello" } }),
        json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" },
                "usage": { "output_tokens": 22 } }),
        json!({ "type": "message_stop" }),
    ]
    .iter()
    .map(|event| {
        format!(
            "event: {}\ndata: {event}\n\n",
            event["type"].as_str().unwrap()
        )
    })
    .collect()
}

async fn fake_upstream(State(state): State<Arc<FakeState>>, request: Request) -> Response {
    let path = request.uri().path().to_owned();
    let authorization = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let letter = letter_of(&authorization);
    let headers: Vec<(String, String)> = request
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_owned(),
                v.to_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    let body = axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .unwrap_or_default();

    match path.as_str() {
        "/oauth/token" => {
            state.refresh_hits.fetch_add(1, Ordering::SeqCst);
            let raw = String::from_utf8_lossy(&body).into_owned();
            let refresh = serde_json::from_str::<Value>(&raw)
                .ok()
                .and_then(|v| {
                    v.get("refresh_token")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .or_else(|| form_value(&raw, "refresh_token"))
                .unwrap_or_default();
            // the letter must survive the refresh; see `letter_of`
            let letter = refresh.trim_start_matches("refresh-").to_owned();
            axum::Json(json!({
                "access_token": format!("tok-{letter}-refreshed"),
                "refresh_token": refresh,
                // Claude's expiry is ExpirySource::ExpiresIn, unlike Codex's
                "expires_in": 3600,
            }))
            .into_response()
        }

        // `limits[]` is preferred over the five_hour/seven_day fallback
        "/api/oauth/usage" => axum::Json(json!({
            "limits": [{
                "kind": "session",
                "group": "session",
                "percent": used_percent(&letter),
                "severity": "normal",
                "resets_at": iso_in_an_hour(),
                "scope": null,
                "is_active": true,
            }],
        }))
        .into_response(),

        // Codex's, so the `/v1/models` collision case has something to serve
        "/wham/usage" => axum::Json(json!({
            "plan_type": "plus",
            "rate_limit": { "primary_window": {
                "used_percent": 10,
                "limit_window_seconds": 5 * 3600,
                "reset_at": Timestamp::now().as_second() + 3600,
            } },
        }))
        .into_response(),

        "/codex/models" => {
            state.codex_model_hits.fetch_add(1, Ordering::SeqCst);
            axum::Json(json!({ "models": [{ "slug": "gpt-5.6-sol" }] })).into_response()
        }

        "/v1/models" => {
            state.anthropic_model_hits.fetch_add(1, Ordering::SeqCst);
            *state.last_headers.lock().unwrap() = headers;
            axum::Json(json!({
                "data": [{ "id": "claude-fable-4-5", "type": "model" }],
                "has_more": false,
            }))
            .into_response()
        }

        "/v1/messages/count_tokens" => {
            state.count_hits.lock().unwrap().push(letter);
            *state.last_headers.lock().unwrap() = headers;
            *state.last_body.lock().unwrap() =
                Some(serde_json::from_slice(&body).unwrap_or(Value::Null));
            // a distinct body shape: no `usage` envelope, no output count
            axum::Json(json!({ "input_tokens": 7 })).into_response()
        }

        "/v1/messages" => {
            state.hits.lock().unwrap().push(letter.clone());
            *state.last_headers.lock().unwrap() = headers;
            let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            *state.last_body.lock().unwrap() = Some(parsed.clone());

            if letter == "F" {
                return (
                    StatusCode::UNAUTHORIZED,
                    axum::Json(json!({ "type": "error",
                                       "error": { "type": "authentication_error",
                                                  "message": "invalid bearer token" } })),
                )
                    .into_response();
            }

            let model = parsed
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();

            // a body problem wearing a quota costume: it must not rotate
            if model == "test-disguised-429" {
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    axum::Json(json!({ "type": "error",
                                       "error": { "type": "rate_limit_error",
                                                  "message": "Error" } })),
                )
                    .into_response();
            }
            if letter == "A" {
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    axum::Json(json!({ "type": "error",
                                       "error": { "type": "rate_limit_error",
                                                  "message": "Monthly usage limit reached" } })),
                )
                    .into_response();
            }

            let mut response = if parsed.get("stream") == Some(&Value::Bool(true)) {
                (
                    StatusCode::OK,
                    [
                        ("content-type", "text/event-stream"),
                        // stale once reqwest decodes the body: must be stripped
                        ("content-encoding", "identity"),
                        ("request-id", "req_stream"),
                    ],
                    message_stream(),
                )
                    .into_response()
            } else {
                (
                    StatusCode::OK,
                    [("request-id", "req_json")],
                    axum::Json(json!({
                        "id": "msg_1",
                        "type": "message",
                        "role": "assistant",
                        "account": letter,
                        "model": parsed.get("model"),
                        "content": [{ "type": "text", "text": "hello" }],
                        "stop_reason": "end_turn",
                        "usage": { "input_tokens": 11, "output_tokens": 22 },
                    })),
                )
                    .into_response()
            };

            let unified = match letter.as_str() {
                "R" => Some("rejected"),
                "H" => Some("allowed"),
                _ => None,
            };
            if let Some(status) = unified {
                let reset = (Timestamp::now() + SignedDuration::from_hours(1)).as_second();
                let out = response.headers_mut();
                out.insert(
                    "anthropic-ratelimit-unified-status",
                    status.parse().unwrap(),
                );
                out.insert(
                    "anthropic-ratelimit-unified-5h-status",
                    status.parse().unwrap(),
                );
                out.insert(
                    "anthropic-ratelimit-unified-5h-reset",
                    reset.to_string().parse().unwrap(),
                );
            }
            response
        }

        _ => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

struct Fixture {
    proxy: Option<ProxyHandle>,
    upstream_shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    state: Arc<ProxyState>,
    fake: Arc<FakeState>,
    base: String,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(tx) = self.upstream_shutdown.take() {
            let _ = tx.send(());
        }
        self.proxy.take();
    }
}

fn sub_for(provider: Provider, letter: &str) -> Sub {
    Sub {
        key: SubKey::new(provider, letter),
        provider,
        label: format!("sub-{letter}"),
        credentials: Credentials {
            plan: None,
            account_id: Some(letter.to_owned()),
            email: None,
            tokens: Tokens {
                access: format!("tok-{letter}"),
                refresh: Some(format!("refresh-{letter}")),
                expires_at: Some(Timestamp::now() + SignedDuration::from_hours(24)),
            },
            source: CredentialSource::Subbier,
        },
    }
}

/// Explicit rather than inherited: the rotation cases depend on both halves.
fn sticky_lowest_usage() -> RouterSettings {
    RouterSettings {
        strategy: StrategyKind::LowestUsage,
        sticky: Some(true),
        auto_switch: true,
        providers_proxied: [true, true],
        usage_deadline: Duration::from_secs(5),
    }
}

impl Fixture {
    async fn start(letters: &[&str]) -> Fixture {
        Fixture::build(letters, &[], sticky_lowest_usage(), None).await
    }

    async fn with_db(letters: &[&str], db: Arc<Db>) -> Fixture {
        Fixture::build(letters, &[], sticky_lowest_usage(), Some(db)).await
    }

    async fn with_codex(letters: &[&str], codex: &[&str]) -> Fixture {
        Fixture::build(letters, codex, sticky_lowest_usage(), None).await
    }

    async fn build(
        letters: &[&str],
        codex: &[&str],
        settings: RouterSettings,
        db: Option<Arc<Db>>,
    ) -> Fixture {
        let fake = Arc::new(FakeState::default());
        let app = axum::Router::new()
            .fallback(fake_upstream)
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let upstream_base = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });

        let tokens = Arc::new(TokenManager::with_token_urls(TokenUrls::all(format!(
            "{upstream_base}/oauth/token"
        ))));
        let state = Arc::new(
            ProxyState::new("127.0.0.1:0".parse().unwrap())
                .with_bases(Bases::all(upstream_base))
                .with_tokens(tokens)
                .with_db(db)
                .with_router(Arc::new(Router::new(settings))),
        );
        let entries: Vec<SubEntry> = letters
            .iter()
            .map(|l| sub_for(Provider::Claude, l))
            .chain(codex.iter().map(|l| sub_for(Provider::Codex, l)))
            .enumerate()
            .map(|(i, sub)| SubEntry::new(SubId(i as u32), sub))
            .collect();
        state.subs.replace(entries);

        let proxy = serve(state.clone()).await.unwrap();
        let base = proxy.base_url();
        Fixture {
            proxy: Some(proxy),
            upstream_shutdown: Some(tx),
            state,
            fake,
            base,
        }
    }

    fn handle(&self) -> &ProxyHandle {
        self.proxy.as_ref().unwrap()
    }

    async fn messages(&self, body: Value) -> reqwest::Response {
        self.post("/v1/messages", body).await
    }

    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        self.post_with_beta(path, body, &[]).await
    }

    /// Each `beta` entry becomes one header line, so a caller can exercise both spellings.
    async fn post_with_beta(&self, path: &str, body: Value, beta: &[&str]) -> reqwest::Response {
        let mut request = libsubby::http::client()
            .post(format!("{}{path}", self.base))
            .header("content-type", "application/json")
            // neither of these client credentials may reach the upstream
            .header("x-api-key", "sk-ant-the-clients-own-key")
            .header("authorization", "Bearer the-clients-own-token");
        for line in beta {
            request = request.header("anthropic-beta", *line);
        }
        request
            .body(serde_json::to_vec(&body).unwrap())
            .send()
            .await
            .unwrap()
    }

    async fn messages_with_beta(&self, body: Value, beta: &[&str]) -> reqwest::Response {
        self.post_with_beta("/v1/messages", body, beta).await
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        libsubby::http::client()
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .unwrap()
    }
}

async fn json_body(response: reqwest::Response) -> Value {
    let text = response.text().await.unwrap();
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}"))
}

fn message(model: &str) -> Value {
    json!({
        "model": model,
        "max_tokens": 64,
        "messages": [{ "role": "user", "content": "hi" }],
    })
}

#[tokio::test]
async fn a_non_streaming_message_is_forwarded_returned_and_counted() {
    let fixture = Fixture::start(&["C"]).await;
    let response = fixture.messages(message("claude-fable-4-5")).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers().get("request-id").unwrap(),
        "req_json",
        "an upstream response header must be forwarded"
    );
    let body = json_body(response).await;
    assert_eq!(body["account"], "C");
    assert_eq!(body["content"][0]["text"], "hello");
    assert_eq!(fixture.fake.hits(), ["C"]);
    // `stream` is forwarded as-is: nothing to force, nothing to aggregate
    assert_eq!(fixture.fake.last_body().get("stream"), None);

    assert_eq!(fixture.state.metrics.proxied_requests_total(SubId(0)), 1);
    assert_eq!(fixture.state.metrics.proxied_in_flight(SubId(0)), 0);
    assert_eq!(
        fixture
            .state
            .metrics
            .proxied_tokens_1h(SubId(0), Timestamp::now()),
        33,
        "11 in + 22 out, off the top-level usage object"
    );
    assert_eq!(fixture.state.last_error(), None);
}

#[tokio::test]
async fn a_streaming_response_passes_through_untouched_and_is_counted() {
    let fixture = Fixture::start(&["C"]).await;
    let mut body = message("claude-fable-4-5");
    body["stream"] = json!(true);
    let response = fixture.messages(body).await;

    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    assert_eq!(response.headers().get("request-id").unwrap(), "req_stream");
    // reqwest has already decoded the body: these describe bytes that are gone
    assert!(response.headers().get("content-encoding").is_none());
    assert!(response.headers().get("content-length").is_none());
    assert_eq!(
        fixture.fake.last_header("accept").as_deref(),
        Some("text/event-stream"),
        "a streaming client asks the upstream for a stream too"
    );

    let text = response.text().await.unwrap();
    assert_eq!(text, message_stream(), "the bytes must be unmodified");
    assert_eq!(fixture.fake.last_body()["stream"], json!(true));

    // the in-flight guard is held to the last byte, not dropped with the headers
    for _ in 0..200 {
        if fixture
            .state
            .metrics
            .proxied_tokens_1h(SubId(0), Timestamp::now())
            > 0
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        fixture
            .state
            .metrics
            .proxied_tokens_1h(SubId(0), Timestamp::now()),
        33,
        "input from message_start, the running output total from message_delta"
    );
}

#[tokio::test]
async fn the_client_auth_headers_are_dropped_and_replaced() {
    let fixture = Fixture::start(&["C"]).await;
    assert_eq!(
        fixture.messages(message("claude-fable-4-5")).await.status(),
        200
    );

    assert_eq!(
        fixture.fake.last_header("authorization").as_deref(),
        Some("Bearer tok-C"),
        "the sub's OAuth bearer replaces the client's"
    );
    assert_eq!(
        fixture.fake.last_header("x-api-key"),
        None,
        "the client's own API key must never reach Anthropic"
    );
    assert_eq!(
        fixture.fake.last_header("anthropic-beta").as_deref(),
        Some("oauth-2025-04-20")
    );
    assert_eq!(
        fixture.fake.last_header("anthropic-version").as_deref(),
        Some("2023-06-01")
    );
    assert_eq!(
        fixture.fake.last_header("user-agent").as_deref(),
        Some("subbier")
    );
    assert_eq!(
        fixture.fake.last_header("accept").as_deref(),
        Some("application/json")
    );
}

// `anthropic-beta` is the one client header merged rather than dropped: forwarding
// a `context_management` field without its beta earned a `400 Extra inputs`.

/// The merged header, split back into tokens.
fn beta_tokens(fixture: &Fixture) -> Vec<String> {
    fixture
        .fake
        .last_header("anthropic-beta")
        .expect("the upstream must always be told which betas apply")
        .split(',')
        .map(|token| token.trim().to_owned())
        .collect()
}

/// Every value the client negotiated must survive in order, and a proxy chain must not double ours.
#[tokio::test]
async fn the_client_betas_are_merged_with_ours_exactly_once_each() {
    let fixture = Fixture::start(&["C"]).await;
    let cases: [(&[&str], &[&str]); 4] = [
        (&[], &["oauth-2025-04-20"]),
        (
            &["context-management-2025-06-27"],
            &["oauth-2025-04-20", "context-management-2025-06-27"],
        ),
        // one comma-separated line and one further line: both spellings at once
        (
            &[
                "context-management-2025-06-27, fine-grained-tool-streaming-2025-05-14",
                "interleaved-thinking-2025-05-14",
            ],
            &[
                "oauth-2025-04-20",
                "context-management-2025-06-27",
                "fine-grained-tool-streaming-2025-05-14",
                "interleaved-thinking-2025-05-14",
            ],
        ),
        (
            &["oauth-2025-04-20,context-management-2025-06-27"],
            &["oauth-2025-04-20", "context-management-2025-06-27"],
        ),
    ];

    for (sent, want) in cases {
        assert_eq!(
            fixture
                .messages_with_beta(message("claude-fable-4-5"), sent)
                .await
                .status(),
            200,
            "{sent:?}"
        );
        assert_eq!(beta_tokens(&fixture), want, "{sent:?}");
    }
}

#[tokio::test]
async fn the_body_field_the_beta_authorises_is_forwarded_unmodified() {
    let fixture = Fixture::start(&["C"]).await;
    let mut body = message("claude-fable-4-5");
    body["context_management"] = json!({
        "edits": [{ "type": "clear_thinking_20251015" }],
    });
    assert_eq!(
        fixture
            .messages_with_beta(body.clone(), &["context-management-2025-06-27"])
            .await
            .status(),
        200
    );
    // the fix is the header, never stripping the field
    assert_eq!(
        fixture.fake.last_body()["context_management"],
        body["context_management"]
    );
}

/// Empirical: the identity block must be `system`'s **first** element, the caller's own kept after.
#[tokio::test]
async fn the_identity_block_is_prepended_to_whatever_system_the_caller_sent() {
    let fixture = Fixture::start(&["C"]).await;
    // `claude` already sends the identity first, so its traffic passes through
    let claude_code = json!([
        { "type": "text", "text": IDENTITY, "cache_control": { "type": "ephemeral" } },
        { "type": "text", "text": "You are an interactive CLI tool…" },
    ]);
    let cases = [
        (Value::Null, json!([{ "type": "text", "text": IDENTITY }])),
        (
            json!("You are a helpful assistant."),
            json!([
                { "type": "text", "text": IDENTITY },
                { "type": "text", "text": "You are a helpful assistant." },
            ]),
        ),
        (
            json!([{ "type": "text", "text": "You are a pirate." }]),
            json!([
                { "type": "text", "text": IDENTITY },
                { "type": "text", "text": "You are a pirate." },
            ]),
        ),
        (claude_code.clone(), claude_code),
    ];

    for (sent, want) in cases {
        let mut body = message("claude-fable-4-5");
        if !sent.is_null() {
            body["system"] = sent.clone();
        }
        assert_eq!(fixture.messages(body).await.status(), 200);
        assert_eq!(fixture.fake.last_body()["system"], want, "system: {sent}");
    }
}

/// Anthropic rejects a body it dislikes with a 429 whose message is the single word "Error".
#[tokio::test]
async fn a_non_self_identifying_429_passes_through_and_never_rotates() {
    let fixture = Fixture::start(&["C", "B"]).await;
    let response = fixture.messages(message("test-disguised-429")).await;

    assert_eq!(response.status(), 429);
    let body = json_body(response).await;
    assert_eq!(
        body["error"]["type"], "rate_limit_error",
        "the upstream body passes through untouched"
    );
    assert_eq!(body["error"]["message"], "Error");

    assert_eq!(
        fixture.fake.hits(),
        ["C"],
        "exactly one account was tried: no other credential can accept the same bytes"
    );
    assert!(fixture.state.router.exhausted_until(SubId(0)).is_none());
    assert!(fixture.state.router.exhausted_until(SubId(1)).is_none());
    assert!(
        fixture.state.router.exhaustions().is_empty(),
        "a request-scoped failure quarantines nothing"
    );
}

#[tokio::test]
async fn a_usage_limit_429_rotates_quarantines_and_bills_the_sub_that_served() {
    // A is at 0% so it is selected first and always answers a genuine usage-limit 429.
    let fixture = Fixture::start(&["A", "C"]).await;
    let response = fixture.messages(message("claude-fable-4-5")).await;

    assert_eq!(response.status(), 200);
    assert_eq!(json_body(response).await["account"], "C");
    assert_eq!(fixture.fake.hits(), ["A", "C"]);
    assert!(
        fixture.state.router.exhausted_until(SubId(0)).is_some(),
        "a real usage limit quarantines the account"
    );
    assert!(fixture.state.router.exhausted_until(SubId(1)).is_none());

    assert_eq!(fixture.state.metrics.proxied_in_flight(SubId(0)), 0, "A");
    assert_eq!(fixture.state.metrics.proxied_in_flight(SubId(1)), 0, "C");
    assert_eq!(fixture.state.metrics.proxied_requests_total(SubId(0)), 1);
    assert_eq!(fixture.state.metrics.proxied_requests_total(SubId(1)), 1);
    assert_eq!(
        fixture
            .state
            .metrics
            .proxied_tokens_1h(SubId(0), Timestamp::now()),
        0,
        "A produced no tokens: it never answered"
    );
    assert_eq!(
        fixture
            .state
            .metrics
            .proxied_tokens_1h(SubId(1), Timestamp::now()),
        33,
        "the tokens belong to the sub that served the request"
    );
}

#[tokio::test]
async fn every_account_used_up_is_a_429_and_no_account_at_all_is_a_503() {
    let fixture = Fixture::start(&["A"]).await;
    let response = fixture.messages(message("claude-fable-4-5")).await;
    assert_eq!(response.status(), 429);
    assert!(
        json_body(response).await["error"]["message"]
            .as_str()
            .unwrap()
            .contains("used up")
    );

    let fixture = Fixture::with_codex(&[], &["C"]).await;
    assert_eq!(
        fixture.messages(message("claude-fable-4-5")).await.status(),
        503,
        "a misconfiguration must never render as a rate limit"
    );
}

/// F answers 401 whatever token it sees, so the retry shows as a second hit before the rotation.
#[tokio::test]
async fn a_401_is_retried_once_and_then_rotates() {
    let fixture = Fixture::start(&["F", "G"]).await;
    let response = fixture.messages(message("claude-fable-4-5")).await;

    assert_eq!(response.status(), 200);
    assert_eq!(json_body(response).await["account"], "G");
    assert_eq!(
        fixture.fake.hits(),
        ["F", "F", "G"],
        "two attempts on F prove the retry, then the rotation"
    );
    assert_eq!(
        fixture.fake.refresh_hits.load(Ordering::SeqCst),
        1,
        "the retry forces exactly one refresh"
    );
}

#[tokio::test]
async fn a_request_that_is_not_a_json_object_is_rejected_before_the_upstream() {
    let fixture = Fixture::start(&["C"]).await;
    let cases: [(&str, &str, u16); 8] = [
        ("text/plain", "{}", 415),
        ("multipart/form-data", "{}", 415),
        ("application/x-www-form-urlencoded", "{}", 415),
        ("application/json", "null", 400),
        ("application/json", "[]", 400),
        ("application/json", "\"a\"", 400),
        ("application/json", "3", 400),
        ("application/json", "{not json", 400),
    ];
    for (content_type, body, want) in cases {
        let response = libsubby::http::client()
            .post(format!("{}/v1/messages", fixture.base))
            .header("content-type", content_type)
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), want, "{content_type} {body}");
    }
    assert!(fixture.fake.hits().is_empty());
}

/// Two subs, two providers, one request: only the sub that served it may move.
#[tokio::test]
async fn a_claude_request_leaves_a_codex_sub_untouched() {
    let fixture = Fixture::with_codex(&["C"], &["C"]).await;
    let (claude, codex) = (SubId(0), SubId(1));

    assert_eq!(
        fixture.messages(message("claude-fable-4-5")).await.status(),
        200
    );

    assert_eq!(fixture.state.metrics.proxied_requests_total(claude), 1);
    assert_eq!(
        fixture
            .state
            .metrics
            .proxied_tokens_1h(claude, Timestamp::now()),
        33
    );
    assert_eq!(
        fixture.state.metrics.proxied_requests_total(codex),
        0,
        "the codex sub served nothing and must count nothing"
    );
    assert_eq!(fixture.state.metrics.proxied_in_flight(codex), 0);
    assert_eq!(
        fixture
            .state
            .metrics
            .proxied_tokens_1h(codex, Timestamp::now()),
        0
    );
    assert_eq!(fixture.state.metrics.last_proxied_at(codex), None);
}

#[tokio::test]
async fn a_proxied_request_records_a_history_row() {
    let dir = std::env::temp_dir().join(format!("subbier-claude-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("state.db");
    let db = Arc::new(Db::open(&path, 30).unwrap());
    let fixture = Fixture::with_db(&["C"], db).await;

    assert_eq!(
        fixture.messages(message("claude-fable-4-5")).await.status(),
        200
    );

    let reader = rusqlite::Connection::open(&path).unwrap();
    let mut row = None;
    for _ in 0..200 {
        row = reader
            .query_row(
                "SELECT sub_key, provider, route, status, input_tokens, output_tokens \
                 FROM proxied_request",
                [],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, Option<i64>>(4)?,
                        r.get::<_, Option<i64>>(5)?,
                    ))
                },
            )
            .ok();
        if row.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (sub, provider, route, status, input_tokens, output_tokens) =
        row.expect("no proxied_request row was written");
    assert_eq!(sub, "claude:C");
    assert_eq!(provider, "claude");
    assert_eq!(route, "/v1/messages");
    assert_eq!(status, 200);
    assert_eq!(input_tokens, Some(11));
    assert_eq!(output_tokens, Some(22));

    drop(fixture);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Billed against the body it predicts, so it must carry the same identity block and the same betas.
#[tokio::test]
async fn count_tokens_forwards_the_same_body_and_returns_a_bare_input_count() {
    let fixture = Fixture::start(&["C"]).await;
    let mut body = message("claude-fable-4-5");
    body["system"] = json!("You are a pirate.");
    let response = fixture
        .post_with_beta(
            "/v1/messages/count_tokens",
            body,
            &["context-management-2025-06-27"],
        )
        .await;

    assert_eq!(response.status(), 200);
    assert_eq!(json_body(response).await, json!({ "input_tokens": 7 }));
    assert_eq!(fixture.fake.count_hits.lock().unwrap().clone(), ["C"]);
    assert_eq!(
        fixture.fake.last_header("authorization").as_deref(),
        Some("Bearer tok-C"),
        "count_tokens still needs a sub, and still gets a fresh header set"
    );
    assert_eq!(fixture.fake.last_body()["system"][0]["text"], IDENTITY);
    assert_eq!(
        beta_tokens(&fixture),
        ["oauth-2025-04-20", "context-management-2025-06-27"]
    );
    // no `usage` envelope and no output count, so only the input lands
    assert_eq!(
        fixture
            .state
            .metrics
            .proxied_tokens_1h(SubId(0), Timestamp::now()),
        7
    );
}

/// The verdict is never in the `/api/oauth/usage` body, only in these headers.
#[tokio::test]
async fn rejected_unified_headers_quarantine_the_sub_at_once() {
    let fixture = Fixture::start(&["R", "C"]).await;
    let response = fixture.messages(message("claude-fable-4-5")).await;
    assert_eq!(response.status(), 200, "the request itself still succeeded");
    assert_eq!(fixture.fake.hits(), ["R"]);

    assert!(
        fixture.state.router.exhausted_until(SubId(0)).is_some(),
        "a rejected window quarantines on the response that mentions it"
    );
    let entry = fixture
        .state
        .usage
        .peek(&SubKey::new(Provider::Claude, "R"))
        .expect("the headers must reach the usage cache");
    let usage = entry.usage.expect("a snapshot, not an error");
    assert_eq!(usage.limit_reached, Some(true));
    assert_eq!(usage.session.unwrap().pct, 100.0);

    // and the next request goes elsewhere
    assert_eq!(
        fixture.messages(message("claude-fable-4-5")).await.status(),
        200
    );
    assert_eq!(fixture.fake.hits(), ["R", "C"]);
}

#[tokio::test]
async fn allowed_unified_headers_refresh_the_snapshot_without_quarantining() {
    let fixture = Fixture::start(&["H"]).await;
    assert_eq!(
        fixture.messages(message("claude-fable-4-5")).await.status(),
        200
    );

    assert!(fixture.state.router.exhausted_until(SubId(0)).is_none());
    let entry = fixture
        .state
        .usage
        .peek(&SubKey::new(Provider::Claude, "H"))
        .expect("the headers must reach the usage cache");
    let usage = entry.usage.expect("a snapshot, not an error");
    assert_eq!(usage.limit_reached, Some(false));
    assert!(usage.session.unwrap().resets_at.is_some());
}

#[tokio::test]
async fn every_route_alias_reaches_the_claude_handler() {
    let fixture = Fixture::start(&["C"]).await;
    for path in [
        "/v1/messages",
        "/messages",
        "/anthropic/v1/messages",
        "/anthropic/messages",
    ] {
        assert_eq!(
            fixture
                .post(path, message("claude-fable-4-5"))
                .await
                .status(),
            200,
            "{path}"
        );
    }
    for path in [
        "/v1/messages/count_tokens",
        "/messages/count_tokens",
        "/anthropic/v1/messages/count_tokens",
        "/anthropic/messages/count_tokens",
    ] {
        assert_eq!(
            fixture
                .post(path, message("claude-fable-4-5"))
                .await
                .status(),
            200,
            "{path}"
        );
    }
    // ANTHROPIC_BASE_URL carries no /v1: `claude` appends the rest itself
    assert_eq!(fixture.handle().anthropic_base_url(), fixture.base);
}

/// Codex keeps the bare `/v1/models`; the Anthropic catalog lives under the explicit alias.
#[tokio::test]
async fn the_anthropic_models_alias_does_not_disturb_the_codex_catalog() {
    let fixture = Fixture::with_codex(&["C"], &["C"]).await;

    let response = fixture.get("/v1/models").await;
    assert_eq!(response.status(), 200);
    let ids: Vec<String> = json_body(response).await["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids, ["gpt-5.6-sol"], "the bare path stays Codex's");
    assert_eq!(fixture.fake.anthropic_model_hits.load(Ordering::SeqCst), 0);

    let response = fixture.get("/anthropic/v1/models").await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        json_body(response).await["data"][0]["id"],
        "claude-fable-4-5"
    );
    assert_eq!(fixture.fake.anthropic_model_hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        fixture.fake.last_header("anthropic-beta").as_deref(),
        Some("oauth-2025-04-20"),
        "the catalog still needs a sub and the OAuth header set"
    );
}
