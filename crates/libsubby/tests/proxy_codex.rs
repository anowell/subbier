//! The Codex (ChatGPT Responses) proxy path, against a local fake upstream, with
//! bases injected rather than set in the environment (`set_var` is a data race).
//! The letter keys the fake: usage 100% for A and E, 40% B, 10% C/G/I, else 0%; A
//! 403s the catalog, D 503s usage, H hangs it, F always 401s, J 401s on the stale
//! `tok-J`, K's refresh 503s. Responses are `resp_1`, … and echo `input` back.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use futures_util::StreamExt;
use jiff::{SignedDuration, Timestamp};
use libsubby::auth::{TokenManager, TokenUrls};
use libsubby::balance::{Router, RouterSettings};
use libsubby::config::{PoolConfig, PoolMember};
use libsubby::model::{
    CredentialSource, Credentials, Provider, StrategyKind, Sub, SubId, SubKey, Tokens, Usage,
    UsageWindow,
};
use libsubby::proxy::{ProxyHandle, ProxyState, SubEntry, serve};
use libsubby::store::db::Db;
use libsubby::store::transcripts::{Limits, TranscriptStore};
use libsubby::usage::Bases;
use serde_json::{Value, json};

#[derive(Default)]
struct FakeState {
    /// `chatgpt-account-id` per `/codex/responses` call, in order.
    hits: Mutex<Vec<String>>,
    last_accept: Mutex<Option<String>>,
    last_body: Mutex<Option<Value>>,
    /// Every body forwarded to `/codex/responses`; `last_body` holds only one.
    bodies: Mutex<Vec<Value>>,
    last_headers: Mutex<Vec<(String, String)>>,
    refresh_hits: AtomicUsize,
    /// `/wham/usage` calls: zero means the ranking was never consulted.
    usage_hits: AtomicUsize,
    model_hits: AtomicUsize,
    model_client_version: Mutex<Option<String>>,
    /// Serves `resp_<n>`, 1-based.
    responses_served: AtomicUsize,
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

    fn bodies(&self) -> Vec<Value> {
        self.bodies.lock().unwrap().clone()
    }

    /// Skip past what an earlier fixture served, so no id is ever reissued.
    fn resume_response_ids_after(&self, served: usize) {
        self.responses_served.store(served, Ordering::SeqCst);
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

/// Codex expiry comes from the access token's own `exp`, not `expires_in`.
fn fresh_access_token() -> String {
    let claims = json!({ "exp": Timestamp::now().as_second() + 3_600 }).to_string();
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims);
    format!("header.{payload}.signature")
}

fn form_value(body: &str, key: &str) -> Option<String> {
    body.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| v.replace('+', " "))
    })
}

fn sse(payload: String) -> Response {
    (
        StatusCode::OK,
        [("content-type", "text/event-stream")],
        payload,
    )
        .into_response()
}

async fn fake_upstream(State(state): State<Arc<FakeState>>, request: Request) -> Response {
    let path = request.uri().path().to_owned();
    let query = request.uri().query().unwrap_or_default().to_owned();
    let account = request
        .headers()
        .get("chatgpt-account-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let accept = request
        .headers()
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let authorization = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
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
            let form = String::from_utf8_lossy(&body).into_owned();
            // Wide enough that two concurrent 401 retries genuinely overlap.
            tokio::time::sleep(Duration::from_millis(30)).await;
            if form_value(&form, "refresh_token").as_deref() == Some("transient-K") {
                return (StatusCode::SERVICE_UNAVAILABLE, "unavailable").into_response();
            }
            axum::Json(json!({
                "access_token": fresh_access_token(),
                "refresh_token": "rotated",
            }))
            .into_response()
        }

        "/codex/models" => {
            state.model_hits.fetch_add(1, Ordering::SeqCst);
            *state.model_client_version.lock().unwrap() = query
                .split('&')
                .find_map(|pair| pair.strip_prefix("client_version=").map(str::to_owned));
            if account == "A" {
                return (StatusCode::FORBIDDEN, "forbidden").into_response();
            }
            // Wider than we model: `codex` reads fields we only pass through.
            axum::Json(json!({
                "models": [
                    {
                        "slug": "gpt-5.6-sol",
                        "display_name": "GPT-5.6 Sol",
                        "supported_reasoning_levels": [{ "effort": "low" }],
                        "base_instructions": "You are Codex.",
                    },
                    { "slug": "gpt-dynamic", "display_name": "Dynamic" },
                ],
            }))
            .into_response()
        }

        "/wham/usage" => {
            state.usage_hits.fetch_add(1, Ordering::SeqCst);
            if account == "D" {
                return (StatusCode::SERVICE_UNAVAILABLE, "usage unavailable").into_response();
            }
            if account == "H" {
                tokio::time::sleep(Duration::from_secs(1)).await;
                return axum::Json(json!({
                    "plan_type": "plus",
                    "rate_limit": { "primary_window": { "used_percent": 0 } },
                }))
                .into_response();
            }
            let used = match account.as_str() {
                "A" | "E" => 100,
                "B" => 40,
                "C" | "G" | "I" => 10,
                _ => 0,
            };
            axum::Json(json!({
                "plan_type": "plus",
                "rate_limit": {
                    "primary_window": {
                        "used_percent": used,
                        "limit_window_seconds": 5 * 3600,
                        "reset_at": Timestamp::now().as_second() + 3600,
                    }
                },
            }))
            .into_response()
        }

        "/codex/responses" => {
            state.hits.lock().unwrap().push(account.clone());
            *state.last_accept.lock().unwrap() = accept;
            *state.last_headers.lock().unwrap() = headers;
            let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            *state.last_body.lock().unwrap() = Some(parsed.clone());
            state.bodies.lock().unwrap().push(parsed.clone());

            if account == "A" {
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    axum::Json(json!({
                        "error": { "message": "Monthly usage limit reached (GoUsageLimitError)" }
                    })),
                )
                    .into_response();
            }
            if account == "B" && parsed.get("fail").is_some() {
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    axum::Json(json!({ "error": { "message": "Monthly usage limit reached" } })),
                )
                    .into_response();
            }
            if account == "F" || (account == "J" && authorization == "Bearer tok-J") {
                return (
                    StatusCode::UNAUTHORIZED,
                    axum::Json(json!({ "error": { "message": "unauthorized" } })),
                )
                    .into_response();
            }
            let model = parsed
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let model = model.as_str();
            // A 429 that says nothing about usage: pass through, do not rotate.
            if model == "test-plain-429" {
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    axum::Json(json!({ "type": "error",
                                "error": { "type": "rate_limit_error", "message": "Error" } })),
                )
                    .into_response();
            }
            if parsed.get("stream") != Some(&Value::Bool(true)) {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(json!({ "detail": "Stream must be set to true" })),
                )
                    .into_response();
            }

            let status = match model {
                "test-incomplete" => "incomplete",
                "test-failed" => "failed",
                _ => "completed",
            };
            let id = format!(
                "resp_{}",
                state.responses_served.fetch_add(1, Ordering::SeqCst) + 1
            );
            let response = json!({
                "id": id,
                "account": account,
                "store": parsed.get("store"),
                "model": parsed.get("model"),
                "status": status,
                "output": [],
                "previous_response_id": null,
                "usage": { "input_tokens": 11, "output_tokens": 22 },
                "error": if status == "failed" {
                    json!({ "code": "server_error", "message": "generation failed" })
                } else { Value::Null },
                "incomplete_details": if status == "incomplete" {
                    json!({ "reason": "max_output_tokens" })
                } else { Value::Null },
            });
            let output_items: Vec<Value> = match model {
                "test-reversed" => vec![
                    json!({ "output_index": 1, "item": { "type": "message", "content": "second" } }),
                    json!({ "output_index": 0, "item": { "type": "message", "content": "first" } }),
                ],
                "test-reasoning" => vec![
                    json!({ "output_index": 0,
                            "item": { "type": "reasoning", "encrypted_content": "encrypted" } }),
                    json!({ "output_index": 1,
                            "item": { "type": "function_call", "call_id": "c1",
                                      "name": "lookup", "arguments": "{}" } }),
                ],
                // The echo lets a case read the spliced transcript off the response.
                _ => {
                    let input = parsed.get("input").and_then(Value::as_array);
                    vec![json!({
                        "output_index": 0,
                        "item": {
                            "type": "message",
                            "account": account,
                            "turns": input.map_or(0, Vec::len),
                        },
                    })]
                }
            };
            let mut events: Vec<String> = if status == "failed" {
                Vec::new()
            } else {
                output_items
                    .iter()
                    .map(|item| {
                        format!(
                            "data: {}",
                            json!({
                                "type": "response.output_item.done",
                                "output_index": item["output_index"],
                                "item": item["item"],
                            })
                        )
                    })
                    .collect()
            };
            events.push(format!(
                "data: {}",
                json!({ "type": format!("response.{status}"), "response": response })
            ));
            events.push("data: [DONE]".to_owned());

            let separator = if model == "test-cr-framing" {
                "\r\r"
            } else {
                "\r\n\r\n"
            };
            let payload = format!("{}{separator}", events.join(separator));

            if model == "test-terminal-open" {
                // Everything, then never closes: return on the terminal event, not EOF.
                let never_ends = futures_util::stream::once(async move {
                    Ok::<Bytes, std::io::Error>(Bytes::from(payload))
                })
                .chain(futures_util::stream::pending());
                return (
                    StatusCode::OK,
                    [("content-type", "text/event-stream")],
                    Body::from_stream(never_ends),
                )
                    .into_response();
            }
            sse(payload)
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

fn sub_for(letter: &str) -> Sub {
    Sub {
        key: SubKey::new(Provider::Codex, letter),
        provider: Provider::Codex,
        label: format!("sub-{letter}"),
        credentials: Credentials {
            plan: None,
            account_id: Some(letter.to_owned()),
            email: None,
            tokens: Tokens {
                access: format!("tok-{letter}"),
                // A missing refresh token is a permanent failure.
                refresh: Some(format!("refresh-{letter}")),
                expires_at: Some(Timestamp::now() + SignedDuration::from_hours(24)),
            },
            source: CredentialSource::Subbier,
        },
    }
}

impl Fixture {
    async fn start(letters: &[&str]) -> Fixture {
        Fixture::with_settings(letters, sticky_lowest_usage()).await
    }

    async fn with_settings(letters: &[&str], settings: RouterSettings) -> Fixture {
        Fixture::build(letters, settings, None, None).await
    }

    async fn with_db(letters: &[&str], db: Arc<Db>) -> Fixture {
        Fixture::build(letters, sticky_lowest_usage(), Some(db), None).await
    }

    /// A store the case owns, so it can outlive the proxy or be sized to evict.
    async fn with_transcripts(letters: &[&str], transcripts: Arc<TranscriptStore>) -> Fixture {
        Fixture::build(letters, sticky_lowest_usage(), None, Some(transcripts)).await
    }

    /// Over the raw TCP upstream, which fails in ways an axum handler cannot.
    async fn against(letters: &[&str], upstream_base: &str, db: Option<Arc<Db>>) -> Fixture {
        Fixture::assemble(
            letters,
            sticky_lowest_usage(),
            db,
            None,
            Arc::new(FakeState::default()),
            upstream_base.to_owned(),
            None,
        )
        .await
    }

    async fn build(
        letters: &[&str],
        settings: RouterSettings,
        db: Option<Arc<Db>>,
        transcripts: Option<Arc<TranscriptStore>>,
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
        Fixture::assemble(
            letters,
            settings,
            db,
            transcripts,
            fake,
            upstream_base,
            Some(tx),
        )
        .await
    }

    async fn assemble(
        letters: &[&str],
        settings: RouterSettings,
        db: Option<Arc<Db>>,
        transcripts: Option<Arc<TranscriptStore>>,
        fake: Arc<FakeState>,
        upstream_base: String,
        upstream_shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    ) -> Fixture {
        let tokens = Arc::new(TokenManager::with_token_urls(TokenUrls::all(format!(
            "{upstream_base}/oauth/token"
        ))));
        let mut proxy_state = ProxyState::new("127.0.0.1:0".parse().unwrap())
            .with_bases(Bases::all(upstream_base))
            .with_tokens(tokens)
            .with_db(db)
            .with_router(Arc::new(Router::new(settings)));
        if let Some(transcripts) = transcripts {
            proxy_state = proxy_state.with_transcripts(transcripts);
        }
        let state = Arc::new(proxy_state);
        state.subs.replace(
            letters
                .iter()
                .enumerate()
                .map(|(i, letter)| SubEntry::new(SubId(i as u32), sub_for(letter))),
        );

        let proxy = serve(state.clone()).await.unwrap();
        let base = proxy.base_url();
        Fixture {
            proxy: Some(proxy),
            upstream_shutdown,
            state,
            fake,
            base,
        }
    }

    fn handle(&self) -> &ProxyHandle {
        self.proxy.as_ref().unwrap()
    }

    async fn responses(&self, body: Value) -> reqwest::Response {
        libsubby::http::client()
            .post(format!("{}/v1/responses", self.base))
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&body).unwrap())
            .send()
            .await
            .unwrap()
    }

    async fn responses_in_pool(&self, pool: &str, body: Value) -> reqwest::Response {
        libsubby::http::client()
            .post(format!("{}/pool/{pool}/v1/responses", self.base))
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&body).unwrap())
            .send()
            .await
            .unwrap()
    }

    /// Warm the usage cache: a pool with no numbers admits everything.
    fn prime_usage(&self, entries: &[(&str, f32, f32)]) {
        for &(letter, session, weekly) in entries {
            self.state.usage.observe(
                &SubKey::new(Provider::Codex, letter),
                Usage {
                    plan: Some("plus".into()),
                    session: Some(UsageWindow::from_pct(session)),
                    weekly: Some(UsageWindow::from_pct(weekly)),
                    ..Usage::default()
                },
            );
        }
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        libsubby::http::client()
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .unwrap()
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

async fn json_body(response: reqwest::Response) -> Value {
    let text = response.text().await.unwrap();
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}"))
}

/// Parsed `data:` payloads, tolerant of both framings and of a half-arrived last line.
fn sse_events(body: &str) -> Vec<Value> {
    body.split(['\r', '\n'])
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|payload| serde_json::from_str(payload).ok())
        .collect()
}

fn completed_id(body: &str) -> Option<String> {
    sse_events(body)
        .iter()
        .find(|event| event["type"] == "response.completed")
        .and_then(|event| event["response"]["id"].as_str().map(str::to_owned))
}

/// The client's own turns in a forwarded `input`: the fake's echoes carry no `role`.
fn spliced_turns(forwarded: &Value) -> Vec<String> {
    forwarded["input"]
        .as_array()
        .unwrap_or_else(|| panic!("no input array in {forwarded}"))
        .iter()
        .filter(|item| item["role"] == "user")
        .map(|item| item["content"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[tokio::test]
async fn models_lists_the_upstream_catalog() {
    let fixture = Fixture::start(&["A", "B", "C"]).await;
    let response = fixture.get("/v1/models").await;
    assert_eq!(response.status(), 200);
    let body = json_body(response).await;
    assert_eq!(body["object"], "list");
    let ids: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["gpt-5.6-sol", "gpt-dynamic"]);
    assert_eq!(
        body["data"][0],
        json!({"id":"gpt-5.6-sol","object":"model","created":0,"owned_by":"openai"})
    );
    // `codex` reads fields we do not model, so its key rides alongside verbatim.
    assert_eq!(
        body["models"],
        json!([
            {
                "slug": "gpt-5.6-sol",
                "display_name": "GPT-5.6 Sol",
                "supported_reasoning_levels": [{ "effort": "low" }],
                "base_instructions": "You are Codex.",
            },
            { "slug": "gpt-dynamic", "display_name": "Dynamic" },
        ])
    );
    assert_eq!(
        fixture.fake.model_client_version.lock().unwrap().as_deref(),
        Some("0.147.0")
    );
    // A answers 403 and is skipped; B serves the catalog.
    assert_eq!(fixture.fake.model_hits.load(Ordering::SeqCst), 2);

    for path in ["/models", "/codex/v1/models", "/codex/models"] {
        assert_eq!(fixture.get(path).await.status(), 200, "{path}");
    }
}

#[tokio::test]
async fn the_model_catalog_is_cached_and_outlives_its_subs() {
    let fixture = Fixture::start(&["A", "B", "C"]).await;
    assert_eq!(fixture.get("/v1/models").await.status(), 200);
    let fetched = fixture.fake.model_hits.load(Ordering::SeqCst);

    let response = fixture.get("/v1/models/gpt-dynamic").await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        json_body(response).await,
        json!({
            "slug": "gpt-dynamic",
            "display_name": "Dynamic",
            "id": "gpt-dynamic",
            "object": "model",
            "created": 0,
            "owned_by": "openai",
        }),
        "the OpenAI keys, over the upstream entry's own fields"
    );

    let unknown = fixture.get("/v1/models/not-a-model").await;
    assert_eq!(unknown.status(), 404);
    let message = json_body(unknown).await["error"]["message"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(message.to_lowercase().contains("not found"), "{message}");

    fixture.state.subs.replace([]);
    let stale = fixture.get("/v1/models").await;
    assert_eq!(stale.status(), 200, "a stale catalog beats a 503");
    let body = json_body(stale).await;
    assert_eq!(body["data"].as_array().unwrap().len(), 2);
    // Both keys survive, so a `codex` whose subs have all gone still gets a decodable body.
    assert_eq!(body["models"][0]["slug"], "gpt-5.6-sol");
    let one = fixture.get("/v1/models/gpt-dynamic").await;
    assert_eq!(one.status(), 200);
    assert_eq!(json_body(one).await["display_name"], "Dynamic");

    assert_eq!(
        fixture.fake.model_hits.load(Ordering::SeqCst),
        fetched,
        "the cached catalog must not be refetched"
    );
}

#[tokio::test]
async fn rotates_to_the_most_available_sub_and_then_stays_on_it() {
    // A = 100% used, B = 40%, C = 10% -> C wins, and stickiness keeps it.
    let fixture = Fixture::start(&["A", "B", "C"]).await;
    for _ in 0..4 {
        let response = fixture
            .responses(json!({ "model": "gpt-5.4", "input": "hi" }))
            .await;
        assert_eq!(response.status(), 200);
        assert_eq!(json_body(response).await["account"], "C");
    }
    assert_eq!(fixture.fake.hits(), ["C", "C", "C", "C"]);
}

#[tokio::test]
async fn aggregates_sse_into_json_unless_the_client_asked_to_stream() {
    for stream in [None, Some(false)] {
        let fixture = Fixture::start(&["C"]).await;
        let mut request = json!({ "model": "gpt-5.4", "input": "x" });
        if let Some(stream) = stream {
            request["stream"] = json!(stream);
        }
        let response = fixture.responses(request).await;
        assert_eq!(response.status(), 200, "{stream:?}");
        assert!(
            response
                .headers()
                .get("content-type")
                .unwrap()
                .to_str()
                .unwrap()
                .contains("application/json"),
            "{stream:?}"
        );
        assert_eq!(
            fixture.fake.last_accept.lock().unwrap().as_deref(),
            Some("text/event-stream"),
            "we always ask upstream for SSE"
        );
        assert_eq!(
            fixture.fake.last_body()["stream"],
            true,
            "upstream is streamed regardless of the client"
        );
        let body = json_body(response).await;
        assert_eq!(body["id"], "resp_1");
        assert_eq!(body["output"].as_array().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn any_terminal_event_ends_the_aggregation() {
    let fixture = Fixture::start(&["C"]).await;
    for (model, status) in [
        ("test-incomplete", "incomplete"),
        ("test-failed", "failed"),
        ("test-cr-framing", "completed"),
        ("test-terminal-open", "completed"),
    ] {
        let response = tokio::time::timeout(
            Duration::from_secs(10),
            fixture.responses(json!({ "model": model, "input": "x" })),
        )
        .await
        .unwrap_or_else(|_| panic!("{model}: waited for an upstream EOF that never comes"));
        assert_eq!(response.status(), 200, "{model}");
        assert_eq!(json_body(response).await["status"], status, "{model}");
    }
}

#[tokio::test]
async fn orders_aggregated_output_by_output_index() {
    let fixture = Fixture::start(&["C"]).await;
    let response = fixture
        .responses(json!({ "model": "test-reversed", "input": "x" }))
        .await;
    let body = json_body(response).await;
    let contents: Vec<&str> = body["output"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["content"].as_str().unwrap())
        .collect();
    assert_eq!(contents, ["first", "second"], "emitted 1 then 0");
}

#[tokio::test]
async fn passes_through_streaming_responses() {
    let fixture = Fixture::start(&["C"]).await;
    let response = fixture
        .responses(json!({ "model": "gpt-5.4", "input": "x", "stream": true }))
        .await;
    assert_eq!(response.status(), 200);
    assert!(
        response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("text/event-stream")
    );
    // reqwest has already decoded the body, so a copied `content-encoding` would lie.
    for stripped in ["content-length", "content-encoding"] {
        assert!(
            !response.headers().contains_key(stripped),
            "{stripped} survived the passthrough"
        );
    }
    assert_eq!(
        fixture.fake.last_accept.lock().unwrap().as_deref(),
        Some("text/event-stream")
    );
    assert!(response.text().await.unwrap().contains("data: [DONE]"));
}

#[tokio::test]
async fn uses_an_account_with_unknown_usage_when_confirmed_accounts_are_exhausted() {
    // E is confirmed at 100%; D's usage endpoint merely fails, which must not quarantine.
    let fixture = Fixture::start(&["E", "D"]).await;
    let response = fixture
        .responses(json!({ "model": "gpt-5.4", "input": "usage fallback" }))
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(json_body(response).await["account"], "D");
}

#[tokio::test]
async fn does_not_let_a_hung_usage_request_block_a_healthy_account() {
    let settings = RouterSettings {
        // A round-level deadline, not a per-request one.
        usage_deadline: Duration::from_millis(20),
        ..sticky_lowest_usage()
    };
    let fixture = Fixture::with_settings(&["H", "I"], settings).await;
    let response = tokio::time::timeout(
        Duration::from_millis(900),
        fixture.responses(json!({ "model": "gpt-5.4", "input": "usage timeout" })),
    )
    .await
    .expect("one hung account blocked the whole round");
    assert_eq!(response.status(), 200);
    assert_eq!(json_body(response).await["account"], "I");
}

#[tokio::test]
async fn deduplicates_concurrent_refreshes_after_401_responses() {
    let fixture = Fixture::start(&["J"]).await;
    let (one, two) = tokio::join!(
        fixture.responses(json!({ "model": "gpt-5.4", "input": "concurrent one" })),
        fixture.responses(json!({ "model": "gpt-5.4", "input": "concurrent two" })),
    );
    assert_eq!(one.status(), 200);
    assert_eq!(two.status(), 200);
    assert_eq!(
        fixture.fake.refresh_hits.load(Ordering::SeqCst),
        1,
        "two concurrent 401 retries must share one token call"
    );
}

#[tokio::test]
async fn does_not_quarantine_accounts_after_transient_refresh_failures() {
    let fixture = Fixture::start(&["K"]).await;
    let established = fixture
        .responses(json!({ "model": "gpt-5.4", "input": "establish sticky account" }))
        .await;
    assert_eq!(json_body(established).await["account"], "K");

    // A refresh token the endpoint answers 503 for, on an expired access token.
    let mut entry = fixture.state.subs.get(SubId(0)).unwrap();
    entry.sub.credentials.tokens = Tokens {
        access: "tok-K".into(),
        refresh: Some("transient-K".into()),
        expires_at: Some(Timestamp::UNIX_EPOCH),
    };
    fixture.state.subs.upsert(entry);

    let failed = fixture
        .responses(json!({ "model": "gpt-5.4", "input": "transient failure" }))
        .await;
    assert_eq!(failed.status(), 502, "a transient refresh failure is a 502");
    assert!(
        fixture.state.router.exhausted_until(SubId(0)).is_none(),
        "a flaky network must NOT quarantine the account"
    );

    let mut entry = fixture.state.subs.get(SubId(0)).unwrap();
    entry.sub.credentials.tokens = Tokens {
        access: "tok-K".into(),
        refresh: None,
        expires_at: Some(Timestamp::now() + SignedDuration::from_hours(1)),
    };
    fixture.state.subs.upsert(entry);

    let recovered = fixture
        .responses(json!({ "model": "gpt-5.4", "input": "retry" }))
        .await;
    assert_eq!(recovered.status(), 200);
    assert_eq!(json_body(recovered).await["account"], "K");
}

#[tokio::test]
async fn fails_over_after_persistent_account_authorization_errors() {
    let fixture = Fixture::start(&["F", "G"]).await;
    let response = fixture
        .responses(json!({ "model": "gpt-5.4", "input": "auth fallback" }))
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(json_body(response).await["account"], "G");
    assert_eq!(
        fixture.fake.hits(),
        ["F", "F", "G"],
        "two attempts on F proves the 401 retry, then rotation to G"
    );
    assert!(
        fixture.state.router.exhausted_until(SubId(0)).is_some(),
        "a second 401 is permanent and quarantines"
    );
}

#[tokio::test]
async fn fails_over_when_the_sticky_sub_is_used_up() {
    let fixture = Fixture::start(&["A", "B"]).await;
    let first = fixture
        .responses(json!({ "model": "gpt-5.4", "input": "y" }))
        .await;
    assert_eq!(json_body(first).await["account"], "B");
    assert_eq!(fixture.fake.hits(), ["B"]);

    let second = fixture
        .responses(json!({ "model": "gpt-5.4", "input": "z", "fail": true }))
        .await;
    assert_eq!(second.status(), 429);
    let message = json_body(second).await["error"]["message"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(message.to_lowercase().contains("used up"), "{message}");
}

#[tokio::test]
async fn rejects_bad_media_types_and_non_object_bodies() {
    let fixture = Fixture::start(&["C"]).await;
    let post = |media_type: &'static str, body: &'static str| {
        let base = fixture.base.clone();
        async move {
            libsubby::http::client()
                .post(format!("{base}/v1/responses"))
                .header("content-type", media_type)
                .body(body)
                .send()
                .await
                .unwrap()
        }
    };

    for media_type in [
        "text/plain",
        "multipart/form-data",
        "application/x-www-form-urlencoded",
        "application/json-whoops",
    ] {
        assert_eq!(post(media_type, "{}").await.status(), 415, "{media_type}");
    }
    for body in ["null", "[]", "\"a string\"", "7", "{oops"] {
        assert_eq!(
            post("application/json; charset=utf-8", body).await.status(),
            400,
            "{body}"
        );
    }
    assert!(fixture.fake.hits().is_empty(), "upstream must be untouched");
}

#[tokio::test]
async fn an_unknown_route_returns_404() {
    let fixture = Fixture::start(&["C"]).await;
    let response = libsubby::http::client()
        .post(format!("{}/v1/chat/completions", fixture.base))
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);

    // Codex subs only, so the Claude routes answer 503 "no candidates".
    for claude_route in ["/v1/messages", "/v1/messages/count_tokens"] {
        let response = libsubby::http::client()
            .post(format!("{}{claude_route}", fixture.base))
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 503, "{claude_route}");
    }
}

#[tokio::test]
async fn normalizes_regular_api_requests_for_the_codex_backend() {
    let fixture = Fixture::start(&["C"]).await;
    let response = fixture
        .responses(json!({
            "model": "gpt-5.5",
            "store": true,
            "prompt_cache_key": "k",
            "prompt_cache_retention": "24h",
            "prompt_cache_options": { "mode": "explicit" },
            "max_output_tokens": 64,
            "input": [
                {
                    "role": "system",
                    "type": "message",
                    "prompt_cache_breakpoint": { "mode": "explicit" },
                    "content": [
                        { "type": "input_text", "text": "sys",
                          "prompt_cache_breakpoint": { "mode": "explicit" } }
                    ]
                },
                { "role": "user", "content": [{ "type": "input_text", "text": "hi" }] }
            ]
        }))
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(json_body(response).await["store"], false);

    let forwarded = fixture.fake.last_body();
    assert_eq!(
        forwarded["store"], false,
        "the backend requires store: false"
    );
    for key in [
        "prompt_cache_key",
        "prompt_cache_retention",
        "prompt_cache_options",
        "max_output_tokens",
    ] {
        assert!(forwarded.get(key).is_none(), "{key} was forwarded");
    }
    let input = forwarded["input"].as_array().unwrap();
    assert_eq!(input[0]["role"], "developer");
    assert!(input[0].get("prompt_cache_breakpoint").is_none());
    assert!(
        input[0]["content"][0]
            .get("prompt_cache_breakpoint")
            .is_none()
    );
    assert_eq!(input[1]["role"], "user");
}

#[tokio::test]
async fn emulates_previous_response_id_chaining_by_inlining_the_cached_transcript() {
    let fixture = Fixture::start(&["C"]).await;
    let first = fixture
        .responses(json!({ "model": "gpt-5.4", "input": "turn one" }))
        .await;
    let first_id = json_body(first).await["id"].as_str().unwrap().to_owned();

    let response = fixture
        .responses(json!({
            "model": "gpt-5.4",
            "previous_response_id": first_id,
            "input": [{ "type": "function_call_output", "call_id": "c1", "output": "ok" }],
        }))
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        json_body(response).await["previous_response_id"],
        first_id.as_str()
    );

    let forwarded = fixture.fake.last_body();
    assert!(
        forwarded.get("previous_response_id").is_none(),
        "the backend holds no state and must never see the key"
    );
    let input = forwarded["input"].as_array().unwrap();
    assert_eq!(input.len(), 3);
    assert_eq!(input[0], json!({ "role": "user", "content": "turn one" }));
    assert_eq!(input[1]["type"], "message");
    assert_eq!(input[2]["type"], "function_call_output");
}

#[tokio::test]
async fn replays_encrypted_reasoning_items_when_chaining() {
    let fixture = Fixture::start(&["C"]).await;
    let first = fixture
        .responses(json!({
            "model": "test-reasoning",
            "input": "investigate",
            "include": ["reasoning.encrypted_content"],
        }))
        .await;
    let first_id = json_body(first).await["id"].as_str().unwrap().to_owned();

    let response = fixture
        .responses(json!({
            "model": "test-reasoning",
            "previous_response_id": first_id,
            "input": [{ "type": "function_call_output", "call_id": "c1", "output": "result" }],
        }))
        .await;
    assert_eq!(response.status(), 200);
    let forwarded = fixture.fake.last_body();
    let input = forwarded["input"].as_array().unwrap();
    assert_eq!(
        input[1],
        json!({ "type": "reasoning", "encrypted_content": "encrypted" }),
        "account-scoped reasoning must be replayed verbatim"
    );
    assert_eq!(input[2]["type"], "function_call");
    assert_eq!(input[3]["type"], "function_call_output");
}

/// Chaining agent loops used to shed turns onto a 400, and a shared response id made two into one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_chains_never_cross_or_shed() {
    const CONVERSATIONS: usize = 40;
    const TURNS: usize = 4;

    let fixture = Fixture::start(&["C"]).await;
    let fixture = &fixture;
    futures_util::future::join_all((1..=CONVERSATIONS).map(|conversation| async move {
        let mut previous: Option<String> = None;
        for turn in 1..=TURNS {
            // Explicit items: a bare string is only lifted on the chained path.
            let mut body = json!({
                "model": "gpt-5.4",
                "input": [{
                    "role": "user",
                    "content": format!("conv {conversation} turn {turn}"),
                }],
            });
            if let Some(id) = &previous {
                body["previous_response_id"] = json!(id);
            }
            let response = fixture.responses(body).await;
            assert_eq!(response.status(), 200, "conv {conversation} turn {turn}");
            let value = json_body(response).await;
            // `turn` client items and `turn - 1` replayed answers.
            assert_eq!(
                value["output"][0]["turns"],
                2 * turn - 1,
                "conv {conversation} turn {turn} was spliced against the wrong depth"
            );
            previous = Some(value["id"].as_str().unwrap().to_owned());
        }
    }))
    .await;

    let bodies = fixture.fake.bodies();
    assert_eq!(bodies.len(), CONVERSATIONS * TURNS);
    let mut seen = HashSet::new();
    for forwarded in &bodies {
        let turns = spliced_turns(forwarded);
        let last = turns.last().expect("a forwarded body with no user turn");
        let (conversation, turn) = match last.split_whitespace().collect::<Vec<_>>()[..] {
            ["conv", c, "turn", t] => (c.to_owned(), t.parse::<usize>().unwrap()),
            _ => panic!("unrecognised turn {last}"),
        };
        let expected: Vec<String> = (1..=turn)
            .map(|t| format!("conv {conversation} turn {t}"))
            .collect();
        assert_eq!(turns, expected, "conv {conversation} turn {turn} crossed");
        assert!(
            seen.insert((conversation.clone(), turn)),
            "conv {conversation} turn {turn} was forwarded twice"
        );
    }
    assert_eq!(seen.len(), CONVERSATIONS * TURNS);
}

/// The old cache was populated by the aggregating path only, so a streaming client never chained.
#[tokio::test]
async fn a_streamed_response_can_be_chained_from() {
    let fixture = Fixture::start(&["C"]).await;
    let first = fixture
        .responses(json!({ "model": "gpt-5.4", "input": "turn one", "stream": true }))
        .await;
    assert_eq!(first.status(), 200);
    let first_id = completed_id(&first.text().await.unwrap()).expect("no terminal frame");

    let second = fixture
        .responses(json!({
            "model": "gpt-5.4", "previous_response_id": first_id, "input": "turn two",
        }))
        .await;
    assert_eq!(second.status(), 200);
    let second = json_body(second).await;
    assert_eq!(second["previous_response_id"], first_id.as_str());
    assert_eq!(
        spliced_turns(&fixture.fake.last_body()),
        ["turn one", "turn two"]
    );
    let second_id = second["id"].as_str().unwrap().to_owned();

    let third = fixture
        .responses(json!({
            "model": "gpt-5.4", "previous_response_id": second_id,
            "input": "turn three", "stream": true,
        }))
        .await;
    assert_eq!(third.status(), 200);
    let third_id = completed_id(&third.text().await.unwrap()).expect("no terminal frame");

    let fourth = fixture
        .responses(json!({
            "model": "gpt-5.4", "previous_response_id": third_id,
            "input": "turn four", "stream": true,
        }))
        .await;
    assert_eq!(fourth.status(), 200);
    let body = fourth.text().await.unwrap();
    assert!(
        body.contains(&format!(r#""previous_response_id":"{third_id}""#)),
        "{body}"
    );
    assert_eq!(
        spliced_turns(&fixture.fake.last_body()),
        ["turn one", "turn two", "turn three", "turn four"]
    );
}

/// The turn must reach the store before the terminal frame naming it leaves.
#[tokio::test]
async fn a_chained_follow_up_immediately_after_a_stream_is_not_a_race() {
    let fixture = Fixture::start(&["C"]).await;
    let first = fixture
        .responses(json!({ "model": "gpt-5.4", "input": "turn one", "stream": true }))
        .await;
    assert_eq!(first.status(), 200);

    let mut stream = first.bytes_stream();
    let mut read = String::new();
    let first_id = loop {
        let chunk = stream
            .next()
            .await
            .expect("the stream ended before its terminal frame")
            .unwrap();
        read.push_str(&String::from_utf8_lossy(&chunk));
        if let Some(id) = completed_id(&read) {
            break id;
        }
    };

    let second = fixture
        .responses(json!({
            "model": "gpt-5.4", "previous_response_id": first_id, "input": "turn two",
        }))
        .await;
    assert_eq!(
        second.status(),
        200,
        "the follow-up outran the store write the terminal frame promised"
    );
    assert_eq!(
        spliced_turns(&fixture.fake.last_body()),
        ["turn one", "turn two"]
    );
}

#[tokio::test]
async fn chains_survive_a_proxy_restart() {
    let dir = std::env::temp_dir().join(format!("subbier-transcripts-{}", uuid::Uuid::new_v4()));
    let path = dir.join("transcripts.db");
    let store = || Arc::new(TranscriptStore::open(&path, Limits::default()).unwrap());

    let fixture = Fixture::with_transcripts(&["C"], store()).await;
    let first = fixture
        .responses(json!({ "model": "gpt-5.4", "input": "turn one" }))
        .await;
    let first_id = json_body(first).await["id"].as_str().unwrap().to_owned();
    let second = fixture
        .responses(json!({
            "model": "gpt-5.4", "previous_response_id": first_id, "input": "turn two",
        }))
        .await;
    let second_id = json_body(second).await["id"].as_str().unwrap().to_owned();
    drop(fixture);

    let restarted = Fixture::with_transcripts(&["C"], store()).await;
    restarted.fake.resume_response_ids_after(2);
    let third = restarted
        .responses(json!({
            "model": "gpt-5.4", "previous_response_id": second_id, "input": "turn three",
        }))
        .await;
    assert_eq!(third.status(), 200);
    assert_eq!(
        json_body(third).await["previous_response_id"],
        second_id.as_str()
    );
    assert_eq!(
        spliced_turns(&restarted.fake.last_body()),
        ["turn one", "turn two", "turn three"]
    );

    drop(restarted);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Replaying half a conversation would be worse than saying it is gone.
#[tokio::test]
async fn chaining_off_an_evicted_transcript_is_a_400() {
    // One filler turn nearly fills the cap, so the root is gone by the second.
    let store = Arc::new(
        TranscriptStore::in_memory(Limits {
            max_bytes: 8_000,
            ttl: Duration::from_secs(3_600),
        })
        .unwrap(),
    );
    let fixture = Fixture::with_transcripts(&["C"], store.clone()).await;

    let first = fixture
        .responses(json!({ "model": "gpt-5.4", "input": "turn one" }))
        .await;
    let first_id = json_body(first).await["id"].as_str().unwrap().to_owned();
    assert!(
        store.contains(&first_id).unwrap(),
        "the root was never stored"
    );

    let mut previous = first_id.clone();
    for filler in ["b", "c"] {
        let response = fixture
            .responses(json!({
                "model": "gpt-5.4",
                "previous_response_id": previous,
                "input": filler.repeat(7_000),
            }))
            .await;
        assert_eq!(response.status(), 200);
        previous = json_body(response).await["id"].as_str().unwrap().to_owned();
    }
    assert!(
        !store.contains(&first_id).unwrap(),
        "the byte cap should have taken the root by now"
    );

    let before = fixture.fake.hits().len();
    let shed = fixture
        .responses(json!({
            "model": "gpt-5.4", "previous_response_id": first_id, "input": "turn four",
        }))
        .await;
    assert_eq!(shed.status(), 400);
    let message = json_body(shed).await["error"]["message"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        message.contains("unknown previous_response_id"),
        "{message}"
    );
    assert_eq!(
        fixture.fake.hits().len(),
        before,
        "a shed request must not reach an account"
    );
}

#[tokio::test]
async fn a_chained_request_prefers_the_sub_that_served_the_previous_turn() {
    // B is 40% used and C is 10%, stickiness off, so the strategy picks C every time.
    let settings = RouterSettings {
        sticky: Some(false),
        ..sticky_lowest_usage()
    };
    let fixture = Fixture::with_settings(&["B", "C"], settings).await;

    // Start the conversation where the ranking would never have sent it.
    fixture.state.router.pin(Some(SubId(0)));
    let first = fixture
        .responses(json!({ "model": "gpt-5.4", "input": "turn one" }))
        .await;
    let first = json_body(first).await;
    assert_eq!(first["account"], "B");
    let first_id = first["id"].as_str().unwrap().to_owned();
    fixture.state.router.pin(None);

    let chained = fixture
        .responses(json!({
            "model": "gpt-5.4", "previous_response_id": first_id, "input": "turn two",
        }))
        .await;
    assert_eq!(chained.status(), 200);
    assert_eq!(
        json_body(chained).await["account"],
        "B",
        "the conversation's account outranks a fresh ranking"
    );

    let unchained = fixture
        .responses(json!({ "model": "gpt-5.4", "input": "a new conversation" }))
        .await;
    assert_eq!(json_body(unchained).await["account"], "C");
    assert_eq!(fixture.fake.hits(), ["B", "B", "C"]);
}

/// A key is a placement, not a chain: the first is routed like any other and later ones follow it.
#[tokio::test]
async fn a_prompt_cache_key_keeps_landing_on_the_account_that_first_served_it() {
    // Both accounts are 10% used and nothing is sticky: unkeyed requests rotate.
    let settings = RouterSettings {
        strategy: StrategyKind::RoundRobin,
        sticky: Some(false),
        ..sticky_lowest_usage()
    };
    let fixture = Fixture::with_settings(&["C", "G"], settings).await;

    for _ in 0..4 {
        let response = fixture
            .responses(json!({ "model": "gpt-5.4", "input": "unkeyed" }))
            .await;
        assert_eq!(response.status(), 200);
    }
    assert_eq!(
        fixture.fake.hits(),
        ["C", "G", "C", "G"],
        "the control: rotation is what a key has to override"
    );

    let mut landed: HashMap<&str, Vec<String>> = HashMap::new();
    for key in ["k1", "k2"] {
        for turn in 1..=4 {
            let response = fixture
                .responses(json!({
                    "model": "gpt-5.4",
                    "input": format!("{key} turn {turn}"),
                    "prompt_cache_key": key,
                }))
                .await;
            assert_eq!(response.status(), 200);
            let account = json_body(response).await["account"]
                .as_str()
                .unwrap()
                .to_owned();
            landed.entry(key).or_default().push(account);
        }
    }
    for (key, accounts) in &landed {
        assert!(
            accounts.iter().all(|account| account == &accounts[0]),
            "{key} was served by {accounts:?}"
        );
    }
    assert!(
        fixture
            .fake
            .bodies()
            .iter()
            .all(|body| body.get("prompt_cache_key").is_none()),
        "routing reads the key, then it is stripped: the backend rejects it"
    );
}

/// A placement follows the strategy rather than overriding it.
#[tokio::test]
async fn a_key_moves_when_its_account_is_used_up_and_stays_moved() {
    let fixture = Fixture::start(&["B", "C"]).await;

    // Start the key somewhere the ranking would never have put it.
    fixture.state.router.pin(Some(SubId(0)));
    let first = fixture
        .responses(json!({
            "model": "gpt-5.4", "input": "one", "prompt_cache_key": "k1",
        }))
        .await;
    assert_eq!(json_body(first).await["account"], "B");
    fixture.state.router.pin(None);

    // B answers `fail` with a usage limit, so this one rotates to C.
    let rotated = fixture
        .responses(json!({
            "model": "gpt-5.4", "input": "two", "prompt_cache_key": "k1", "fail": true,
        }))
        .await;
    assert_eq!(rotated.status(), 200);
    assert_eq!(json_body(rotated).await["account"], "C");

    let after = fixture
        .responses(json!({
            "model": "gpt-5.4", "input": "three", "prompt_cache_key": "k1",
        }))
        .await;
    assert_eq!(json_body(after).await["account"], "C");
    assert_eq!(
        fixture.fake.hits(),
        ["B", "B", "C", "C"],
        "the last request never attempted the account the key had moved off"
    );
}

/// Hopping a chain loses account-scoped reasoning items; hopping a key loses only cache warmth.
#[tokio::test]
async fn chain_affinity_outranks_key_placement() {
    let fixture = Fixture::start(&["B", "C"]).await;

    // C is the emptier account, so the ranking places the key there.
    let placed = fixture
        .responses(json!({
            "model": "gpt-5.4", "input": "one", "prompt_cache_key": "k1",
        }))
        .await;
    assert_eq!(json_body(placed).await["account"], "C");

    // Deliberately unkeyed: a key would be re-placed onto B, leaving nothing to disagree about.
    fixture.state.router.pin(Some(SubId(0)));
    let started = fixture
        .responses(json!({ "model": "gpt-5.4", "input": "two" }))
        .await;
    let started = json_body(started).await;
    assert_eq!(started["account"], "B");
    let previous = started["id"].as_str().unwrap().to_owned();
    fixture.state.router.pin(None);

    let both = fixture
        .responses(json!({
            "model": "gpt-5.4",
            "input": "three",
            "prompt_cache_key": "k1",
            "previous_response_id": previous,
        }))
        .await;
    assert_eq!(both.status(), 200);
    assert_eq!(
        json_body(both).await["account"],
        "B",
        "the conversation's account outranks the key's"
    );
}

/// Placements share the turns' file, and answer before the ranking is consulted.
#[tokio::test]
async fn placements_survive_a_proxy_restart() {
    let dir = std::env::temp_dir().join(format!("subbier-placements-{}", uuid::Uuid::new_v4()));
    let path = dir.join("transcripts.db");
    let store = || Arc::new(TranscriptStore::open(&path, Limits::default()).unwrap());

    let fixture = Fixture::with_transcripts(&["B", "C"], store()).await;
    let first = fixture
        .responses(json!({
            "model": "gpt-5.4", "input": "one", "prompt_cache_key": "k1",
        }))
        .await;
    assert_eq!(
        json_body(first).await["account"],
        "C",
        "the emptier account: the ranking chose the placement"
    );
    assert!(fixture.fake.usage_hits.load(Ordering::SeqCst) > 0);
    drop(fixture);

    let restarted = Fixture::with_transcripts(&["B", "C"], store()).await;
    restarted.fake.resume_response_ids_after(1);
    let again = restarted
        .responses(json!({
            "model": "gpt-5.4", "input": "two", "prompt_cache_key": "k1",
        }))
        .await;
    assert_eq!(again.status(), 200);
    assert_eq!(json_body(again).await["account"], "C");
    assert_eq!(restarted.fake.hits(), ["C"]);
    assert_eq!(
        restarted.fake.usage_hits.load(Ordering::SeqCst),
        0,
        "a known placement returns before the usage round the ranking needs"
    );

    drop(restarted);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A turn stores its own items and never the conversation in front of them.
#[tokio::test]
async fn transcripts_grow_linearly_not_quadratically() {
    let fixture = Fixture::start(&["C"]).await;

    let mut previous: Option<String> = None;
    let mut deltas = 0usize;
    for turn in 1..=6 {
        let input = format!("turn {turn} {}", "x".repeat(200));
        let mut body = json!({ "model": "gpt-5.4", "input": &input });
        if let Some(id) = &previous {
            body["previous_response_id"] = json!(id);
        }
        let response = fixture.responses(body).await;
        assert_eq!(response.status(), 200);
        let value = json_body(response).await;
        deltas += serde_json::to_string(&json!([{ "role": "user", "content": input }]))
            .unwrap()
            .len()
            + serde_json::to_string(&value["output"]).unwrap().len();
        previous = Some(value["id"].as_str().unwrap().to_owned());
    }

    assert_eq!(fixture.state.transcripts.len().unwrap(), 6);
    let bytes = fixture.state.transcripts.bytes().unwrap();
    assert!(
        bytes < 2 * deltas as u64,
        "{bytes} bytes stored for {deltas} bytes of deltas: a full transcript per turn"
    );
}

#[tokio::test]
async fn every_client_header_is_dropped_and_a_fresh_set_is_built() {
    let fixture = Fixture::start(&["C"]).await;
    let response = libsubby::http::client()
        .post(format!("{}/v1/responses", fixture.base))
        .header("content-type", "application/json")
        .header("authorization", "Bearer client-secret")
        .header("x-client-header", "leak-me")
        .header("cookie", "session=leak-me")
        .header("openai-organization", "leak-me")
        .body(serde_json::to_vec(&json!({ "model": "gpt-5.4", "input": "x" })).unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    let sent: HashMap<String, String> = fixture
        .fake
        .last_headers
        .lock()
        .unwrap()
        .iter()
        .cloned()
        .collect();
    for leaked in ["x-client-header", "cookie", "openai-organization"] {
        assert!(!sent.contains_key(leaked), "{leaked} reached the upstream");
    }
    assert_eq!(sent["authorization"], "Bearer tok-C");
    assert_eq!(sent["chatgpt-account-id"], "C");
    assert_eq!(sent["accept"], "text/event-stream");
    assert_eq!(sent["openai-beta"], "responses=experimental");
    assert_eq!(sent["originator"], "subbier");
    assert_eq!(sent["user-agent"], "subbier");
    assert_eq!(
        sent["session-id"], sent["x-client-request-id"],
        "one uuid, two headers"
    );
    assert_eq!(sent["session-id"].len(), 36, "hyphenated uuid v4");

    let mut entry = fixture.state.subs.get(SubId(0)).unwrap();
    entry.sub.credentials.account_id = None;
    fixture.state.subs.upsert(entry);
    let response = fixture
        .responses(json!({ "model": "gpt-5.4", "input": "x" }))
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        fixture.fake.last_header("chatgpt-account-id").as_deref(),
        Some(""),
        "an account with no id sends the header empty, not omitted"
    );
}

#[tokio::test]
async fn a_429_rotates_and_quarantines_only_when_it_names_a_usage_limit() {
    let plain = Fixture::start(&["C", "G"]).await;
    let response = plain
        .responses(json!({ "model": "test-plain-429", "input": "x" }))
        .await;
    assert_eq!(response.status(), 429, "the upstream status passes through");
    assert_eq!(
        json_body(response).await["error"]["type"],
        "rate_limit_error",
        "and so does the upstream body"
    );
    assert_eq!(
        plain.fake.hits().len(),
        1,
        "rotating would burn one account per candidate for one bad request"
    );
    for id in [SubId(0), SubId(1)] {
        assert!(
            plain.state.router.exhausted_until(id).is_none(),
            "{id} was quarantined for a 429 that never mentioned usage"
        );
    }

    let usage = Fixture::start(&["A", "C"]).await;
    // A is at 100% and the usage round would skip it, so pin it.
    usage.state.router.pin(Some(SubId(0)));
    let response = usage
        .responses(json!({ "model": "gpt-5.4", "input": "x" }))
        .await;
    assert_eq!(response.status(), 200, "it failed over to C");
    assert_eq!(json_body(response).await["account"], "C");
    assert_eq!(usage.fake.hits(), ["A", "C"]);
    assert!(
        usage.state.router.exhausted_until(SubId(0)).is_some(),
        "a real usage limit quarantines"
    );
}

#[tokio::test]
async fn a_proxied_request_moves_only_the_serving_subs_counters() {
    for stream in [false, true] {
        let fixture = Fixture::start(&["C"]).await;
        // Only a correctly captured `SubId` keeps a Claude sub's counters out of a Codex request.
        let claude = SubId(9);
        fixture.state.subs.upsert(SubEntry::new(
            claude,
            Sub {
                key: SubKey::new(Provider::Claude, "C"),
                provider: Provider::Claude,
                ..sub_for("C")
            },
        ));
        assert_eq!(fixture.state.metrics.proxied_requests_total(SubId(0)), 0);

        let response = fixture
            .responses(json!({ "model": "gpt-5.4", "input": "x", "stream": stream }))
            .await;
        assert_eq!(response.status(), 200, "stream: {stream}");
        // A streamed body is counted as it is consumed.
        let _ = response.text().await.unwrap();
        for _ in 0..200 {
            if fixture.state.metrics.proxied_in_flight(SubId(0)) == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert_eq!(fixture.state.metrics.proxied_requests_total(SubId(0)), 1);
        assert_eq!(fixture.state.metrics.proxied_in_flight(SubId(0)), 0);
        assert_eq!(
            fixture
                .state
                .metrics
                .proxied_tokens_1h(SubId(0), Timestamp::now()),
            33,
            "stream: {stream}: 11 in + 22 out, off the terminal event"
        );
        assert_eq!(fixture.state.last_error(), None);

        assert_eq!(
            fixture.state.metrics.proxied_requests_total(claude),
            0,
            "the claude sub served nothing and must count nothing"
        );
        assert_eq!(fixture.state.metrics.proxied_in_flight(claude), 0);
        assert_eq!(
            fixture
                .state
                .metrics
                .proxied_tokens_1h(claude, Timestamp::now()),
            0
        );
        assert_eq!(fixture.state.metrics.last_proxied_at(claude), None);
    }
}

/// A keeps only the attempt it made; every counter the request produced is C's.
#[tokio::test]
async fn a_failover_attributes_the_request_to_the_sub_that_served_it() {
    let fixture = Fixture::start(&["A", "C"]).await;
    // A answers a genuine usage-limit 429; pin it so the usage round does not skip it.
    fixture.state.router.pin(Some(SubId(0)));

    let response = fixture
        .responses(json!({ "model": "gpt-5.4", "input": "x" }))
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(json_body(response).await["account"], "C");
    assert_eq!(fixture.fake.hits(), ["A", "C"]);

    assert_eq!(fixture.state.metrics.proxied_in_flight(SubId(0)), 0, "A");
    assert_eq!(fixture.state.metrics.proxied_in_flight(SubId(1)), 0, "C");
    assert_eq!(
        fixture.state.metrics.proxied_requests_total(SubId(0)),
        1,
        "A was attempted once, and that attempt is A's"
    );
    assert_eq!(
        fixture.state.metrics.proxied_requests_total(SubId(1)),
        1,
        "the attempt that served the client is C's"
    );
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
async fn a_failover_writes_each_attempts_row_against_its_own_sub() {
    let dir = std::env::temp_dir().join(format!("subbier-proxy-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("state.db");
    let db = Arc::new(Db::open(&path, 30).unwrap());
    let fixture = Fixture::with_db(&["A", "C"], db).await;
    fixture.state.router.pin(Some(SubId(0)));

    let response = fixture
        .responses(json!({ "model": "gpt-5.4", "input": "x" }))
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(json_body(response).await["account"], "C");

    // The writer thread is fire-and-forget, so poll for both rows.
    let reader = rusqlite::Connection::open(&path).unwrap();
    let mut rows: Vec<(String, i64, Option<i64>)> = Vec::new();
    for _ in 0..200 {
        rows = reader
            .prepare("SELECT sub_key, status, output_tokens FROM proxied_request ORDER BY status")
            .unwrap()
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        if rows.len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        rows,
        vec![
            ("codex:C".to_owned(), 200, Some(22)),
            ("codex:A".to_owned(), 429, None),
        ],
        "the served row is C's and the failed attempt stays A's"
    );
    let served: (String, String) = reader
        .query_row(
            "SELECT provider, route FROM proxied_request WHERE status = 200",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(served, ("codex".to_owned(), "/v1/responses".to_owned()));

    drop(fixture);
    let _ = std::fs::remove_dir_all(&dir);
}

fn pool(name: &str, subs: &[&str], session: f32, weekly: f32) -> PoolConfig {
    PoolConfig {
        name: name.to_string(),
        provider: Some(Provider::Codex),
        subs: subs.iter().map(|s| PoolMember::any(*s)).collect(),
        max_sub_session_utilization: session,
        max_sub_weekly_utilization: weekly,
    }
}

#[tokio::test]
async fn a_pooled_request_never_reaches_an_account_outside_its_pool() {
    let fixture = Fixture::start(&["A", "B", "C"]).await;
    fixture
        .state
        .set_pools(vec![pool("moonshot", &["sub-A", "sub-B"], 1.0, 1.0)]);

    // Pools are an additional door, not a narrowing: plain routing still reaches C,
    // which a pool of {A, B} must avoid even though C is now the sticky account.
    let unpooled = fixture
        .responses(json!({ "model": "gpt-5.4", "input": "hi" }))
        .await;
    assert_eq!(json_body(unpooled).await["account"], "C");

    for _ in 0..3 {
        let response = fixture
            .responses_in_pool("moonshot", json!({ "model": "gpt-5.4", "input": "hi" }))
            .await;
        assert_eq!(response.status(), 200);
        assert_ne!(json_body(response).await["account"], "C");
    }
    let pooled = &fixture.fake.hits()[1..];
    assert!(
        !pooled.iter().any(|hit| hit == "C"),
        "C was never in the pool: {pooled:?}"
    );
}

#[tokio::test]
async fn a_pool_ceiling_skips_full_members_and_429s_when_all_are_full() {
    // The ceiling is 50% of the week: B is 60% into its, C is 20%.
    let fixture = Fixture::start(&["B", "C"]).await;
    fixture.prime_usage(&[("B", 10.0, 60.0), ("C", 10.0, 20.0)]);
    fixture
        .state
        .set_pools(vec![pool("moonshot", &["sub-B", "sub-C"], 1.0, 0.5)]);
    let response = fixture
        .responses_in_pool("moonshot", json!({ "model": "gpt-5.4", "input": "hi" }))
        .await;
    assert_eq!(json_body(response).await["account"], "C");
    assert_eq!(fixture.fake.hits(), ["C"]);

    let full = Fixture::start(&["B", "C"]).await;
    full.prime_usage(&[("B", 10.0, 60.0), ("C", 10.0, 80.0)]);
    full.state
        .set_pools(vec![pool("moonshot", &["sub-B", "sub-C"], 1.0, 0.5)]);
    let response = full
        .responses_in_pool("moonshot", json!({ "model": "gpt-5.4", "input": "hi" }))
        .await;
    assert_eq!(response.status(), 429);
    let body = json_body(response).await;
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("reserved for other pools"), "{message}");
    assert!(full.fake.hits().is_empty(), "no account was touched");
}

#[tokio::test]
async fn a_url_naming_an_unconfigured_pool_is_a_404_not_a_free_pass() {
    // A typo in a base URL must never quietly hand an experiment every account.
    let fixture = Fixture::start(&["A", "B", "C"]).await;
    fixture
        .state
        .set_pools(vec![pool("moonshot", &["sub-A"], 1.0, 1.0)]);

    let response = fixture
        .responses_in_pool("moonshoot", json!({ "model": "gpt-5.4", "input": "hi" }))
        .await;
    assert_eq!(response.status(), 404);
    assert!(fixture.fake.hits().is_empty(), "no account was touched");
}

#[tokio::test]
async fn a_pool_serves_the_model_catalog_too() {
    // A pool is a base URL, so everything the bare proxy answers it answers.
    let fixture = Fixture::start(&["C"]).await;
    fixture
        .state
        .set_pools(vec![pool("moonshot", &["sub-C"], 1.0, 1.0)]);

    let response = fixture.get("/pool/moonshot/v1/models").await;
    assert_eq!(response.status(), 200);
    let body = json_body(response).await;
    assert_eq!(body["object"], "list");
    assert!(body["data"].as_array().is_some_and(|d| !d.is_empty()));

    let one = fixture.get("/pool/moonshot/v1/models/gpt-dynamic").await;
    assert_eq!(one.status(), 200);
    assert_eq!(json_body(one).await["id"], "gpt-dynamic");
}

#[tokio::test]
async fn the_pool_base_urls_are_the_ones_a_client_is_told_to_use() {
    let fixture = Fixture::start(&["C"]).await;
    let handle = fixture.handle();
    assert_eq!(
        handle.pool_openai_base_url("moonshot"),
        format!("{}/pool/moonshot/v1", fixture.base)
    );
    // `claude` appends `/v1/messages` itself, so this one carries no `/v1`.
    assert_eq!(
        handle.pool_anthropic_base_url("moonshot"),
        format!("{}/pool/moonshot", fixture.base)
    );
}

/// How a [`RawUpstream`] mistreats its first `n` `/codex/responses` calls.
#[derive(Clone, Copy)]
enum Flake {
    /// Read the whole request and close without answering a byte.
    Silence(usize),
    /// Answer 200, send one non-terminal event, then close short of `content-length`.
    Truncate(usize),
}

/// Fails the transport rather than the protocol: the axum fake can only ever answer.
struct RawUpstream {
    base: String,
    /// `/codex/responses` connections accepted, whatever became of them.
    attempts: Arc<AtomicUsize>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for RawUpstream {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

impl RawUpstream {
    async fn start(flake: Flake) -> RawUpstream {
        let listener = tokio::net::TcpListener::bind::<SocketAddr>("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let attempts = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();

        let counter = attempts.clone();
        tokio::spawn(async move {
            let serve = async move {
                while let Ok((socket, _)) = listener.accept().await {
                    let counter = counter.clone();
                    tokio::spawn(async move { serve_raw(socket, flake, counter).await });
                }
            };
            tokio::select! {
                () = serve => {}
                _ = rx => {}
            }
        });

        RawUpstream {
            base,
            attempts,
            shutdown: Some(tx),
        }
    }

    fn attempts(&self) -> usize {
        self.attempts.load(Ordering::SeqCst)
    }
}

async fn serve_raw(mut socket: tokio::net::TcpStream, flake: Flake, counter: Arc<AtomicUsize>) {
    use tokio::io::AsyncWriteExt;

    let Some(head) = read_request(&mut socket).await else {
        return;
    };
    if !head.starts_with("POST /codex/responses") {
        let _ = socket
            .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            .await;
        return;
    }
    let attempt = counter.fetch_add(1, Ordering::SeqCst) + 1;
    match flake {
        Flake::Silence(n) if attempt <= n => (),
        Flake::Truncate(n) if attempt <= n => {
            let body = format!(
                "data: {}\r\n\r\n",
                json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": { "type": "message", "content": "half" },
                })
            );
            let _ = socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len() + 64,
                    )
                    .as_bytes(),
                )
                .await;
        }
        _ => {
            let body = completed_sse();
            let _ = socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len(),
                    )
                    .as_bytes(),
                )
                .await;
        }
    }
}

/// Reads the body too, so the upstream has the whole upload before it fails.
async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<String> {
    use tokio::io::AsyncReadExt;

    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let head_end = buffer
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|at| at + 4);
        if let Some(head_end) = head_end {
            let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
            let length: usize = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().ok())?
                })
                .unwrap_or(0);
            while buffer.len() < head_end + length {
                match socket.read(&mut chunk).await {
                    Ok(0) | Err(_) => return None,
                    Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                }
            }
            return Some(head);
        }
        match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
        }
    }
}

fn completed_sse() -> String {
    let response = json!({
        "id": "resp_raw",
        "status": "completed",
        "output": [],
        "usage": { "input_tokens": 11, "output_tokens": 22 },
    });
    format!(
        "data: {}\r\n\r\ndata: [DONE]\r\n\r\n",
        json!({ "type": "response.completed", "response": response })
    )
}

async fn error_message(response: reqwest::Response) -> String {
    json_body(response).await["error"]["message"]
        .as_str()
        .expect("an error body")
        .to_owned()
}

#[tokio::test]
async fn a_transport_death_before_any_response_is_resent_to_the_same_sub() {
    let upstream = RawUpstream::start(Flake::Silence(2)).await;
    let fixture = Fixture::against(&["C"], &upstream.base, None).await;

    let response = fixture
        .responses(json!({ "model": "gpt-5.4", "input": "x" }))
        .await;

    assert_eq!(response.status(), 200);
    assert_eq!(json_body(response).await["id"], "resp_raw");
    assert_eq!(upstream.attempts(), 3, "two resends, then the answer");
    assert_eq!(
        fixture.state.metrics.proxied_requests_total(SubId(0)),
        1,
        "the resends are one routed request, not three"
    );
}

/// Past the resend budget the client gets the whole `source()` chain, not just reqwest's one line.
#[tokio::test]
async fn a_transport_that_never_answers_gives_the_client_the_error_chain() {
    let upstream = RawUpstream::start(Flake::Silence(usize::MAX)).await;
    let fixture = Fixture::against(&["C"], &upstream.base, None).await;

    let response = fixture
        .responses(json!({ "model": "gpt-5.4", "input": "x" }))
        .await;

    assert_eq!(response.status(), 502);
    assert!(upstream.attempts() > 1, "the request was never resent");
    let message = error_message(response).await;
    let tail = message
        .split_once("codex/responses)")
        .unwrap_or_else(|| panic!("not reqwest's message: {message}"))
        .1;
    assert!(
        tail.starts_with(": "),
        "the source chain is missing: {message}"
    );
}

/// A death after 200 headers but before the client has a byte is worth one more upstream call.
#[tokio::test]
async fn a_response_that_dies_mid_body_is_asked_for_once_more() {
    let upstream = RawUpstream::start(Flake::Truncate(1)).await;
    let fixture = Fixture::against(&["C"], &upstream.base, None).await;

    let response = fixture
        .responses(json!({ "model": "gpt-5.4", "input": "x" }))
        .await;

    assert_eq!(response.status(), 200);
    assert_eq!(json_body(response).await["id"], "resp_raw");
    assert_eq!(upstream.attempts(), 2, "the truncated one, then the answer");
}

/// A stream the client has bytes of cannot be retried, so the row says 502, not the promised 200.
#[tokio::test]
async fn a_stream_that_dies_after_the_client_has_bytes_is_recorded_as_a_502() {
    let dir = std::env::temp_dir().join(format!("subbier-proxy-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("state.db");
    let db = Arc::new(Db::open(&path, 30).unwrap());

    let upstream = RawUpstream::start(Flake::Truncate(usize::MAX)).await;
    let fixture = Fixture::against(&["C"], &upstream.base, Some(db)).await;

    let response = fixture
        .responses(json!({ "model": "gpt-5.4", "input": "x", "stream": true }))
        .await;
    assert_eq!(response.status(), 200, "the headers were already good");
    let mut stream = response.bytes_stream();
    let mut failed = false;
    while let Some(chunk) = stream.next().await {
        if chunk.is_err() {
            failed = true;
        }
    }
    assert!(failed, "the truncated body ended cleanly");

    // The writer thread is fire-and-forget, so poll for the row.
    let reader = rusqlite::Connection::open(&path).unwrap();
    let mut status = None;
    for _ in 0..200 {
        status = reader
            .query_row("SELECT status FROM proxied_request", [], |r| {
                r.get::<_, i64>(0)
            })
            .ok();
        if status.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        status.expect("no proxied_request row was written"),
        502,
        "a stream that died is not a 200"
    );
    assert_eq!(upstream.attempts(), 1, "a started stream is never resent");

    drop(fixture);
    let _ = std::fs::remove_dir_all(&dir);
}
