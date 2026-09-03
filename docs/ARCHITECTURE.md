# subbier — architecture

How subbier is put together. The rules it is built to are
[`PRINCIPLES.md`](PRINCIPLES.md); the macOS menu is
[`MENU-DESIGN.md`](MENU-DESIGN.md). This page explains the shapes; the code
explains itself.

## The big picture

subbier is one background engine and some thin frontends.

The engine finds the Codex and Claude accounts you are already logged into
("subs"), polls each one's usage, and runs a local proxy that spreads
`codex`/`claude` traffic across them. Frontends draw what the engine knows and
send it instructions. They never compute anything.

```
  ~/.codex/auth.json ─┐                                       ┌──────────────────┐
  Claude Keychain    ─┼─ discovery ─┐                          │ proxy  :8787     │
  ~/.subbier/subs.json┘             ├─ subs ── usage poller ── │  Codex + Claude  │
                                    │                          │  balancer        │
  ~/.subbier/config.kdl ── config ──┴──────── Engine ──────────┴──────────────────┘
                                                │
                              Snapshot (out)    │    Command (in)
                                                │
                     ┌──────────────────────────┴──────────────────────────┐
              subbier-menubar                                        subbier
              macOS menu bar app                  status · watch · serve · login · env
```

Five ideas carry the whole design:

1. **Snapshot out, Command in.** The engine publishes an immutable `Snapshot`
   of everything it knows. A frontend draws it and sends back a `Command`.
   Commands never return values: the result shows up in the next snapshot.
   That is the entire contract between the library and every frontend.

2. **Two kinds of number, never mixed.** *Allowance* is what the provider
   says an account has used, and it counts everything, including runs that
   never touched subbier. *Proxy metrics* count only what subbier carried,
   and belong to the endpoint that carried them, not the account. subbier
   shows both, labelled, and never derives one from the other.

3. **Adopt, don't take over.** subbier reads the credentials `codex` and
   `claude` already have, keeps its own refreshed copy, and never writes back.
   A machine with both CLIs logged in works with no config and no login.

4. **Routing is a filter and a pipeline.** Candidates are filtered (provider,
   enabled, not exhausted, in the pool), then the first rule that applies
   wins: user pin, conversation affinity, cache-key placement, stickiness,
   strategy. A pool is a filter, so it composes with every strategy.

5. **One process owns the port.** The proxy port is how a second subbier
   finds the first. `serve` binds; everything else asks the running instance
   over `GET /status` and draws what it gets back.

The rest of this file takes each of those a level deeper.

---

## 1. Snapshot and Command

The `Snapshot` is a cheap-to-clone, thread-safe, serialisable value published
on a watch channel. A watch channel, not an event stream, because a frontend is
a projection of current state, not a log: bursts collapse, nothing queues, and
a frontend that was busy skips straight to the newest value.

What a snapshot carries: every sub with its windows (percent, reset time,
severity, projection) and health; every pool; the proxy's state and its
ready-to-paste base URLs; the settings; an overall percentage; the worst
severity; any threshold crossings since last time; and any login in progress.

Three rules keep the boundary honest:

- **Numbers and enums, not rendered strings.** A snapshot says 67 percent and
  `Warn`. How wide the bar is and what colour `Warn` means is the frontend's.
  The library offers formatting helpers (bar, percent, duration, tokens) one
  field at a time and never a composed row.
- **The library does not know what a menu or a terminal is.** Platform side
  effects, like writing a LaunchAgent plist, live in the frontend.
- **The snapshot is also a wire format.** `subbier status` reads it off a
  running instance that may be an older build, so every new field defaults
  when missing. A parse failure must never masquerade as "nothing running".

Commands are the menu's toggles (proxy on, strategy, sticky, per-provider,
per-sub, notifications, menu bar style, launch at login) and a few actions
(pin, refresh, clear exhaustion, rediscover, login, remove, reload config,
shutdown).

## 2. Two kinds of number

| | Allowance | Proxy metrics |
|---|---|---|
| Comes from | the provider's usage API | subbier's own proxy |
| Counts | every request on the account | only requests subbier routed |
| Belongs to | an account | an endpoint: the bare proxy, or one pool |
| Answers | how close am I to the limit | what is flowing through right now |

The two legitimately disagree: an allowance bar climbing while proxied tokens
stay flat means traffic is going around the proxy. Nothing reconciles them.

Consequences that hold everywhere:

- The allowance bar always comes from the provider. If the poll fails the bar
  goes stale, it is not reconstructed from token counts.
- Proxy numbers carry their provenance in the name (`proxied_tokens_1h`), in
  the schema (`proxied_request`), and on screen (a `via proxy` block or row).
- A pool's numbers are its own endpoint's, never its members summed. A member
  normally serves other endpoints too.
- No log scraping. Session files could only reconstruct traffic that went
  around the proxy, and the allowance API already accounts for that.

## 3. Accounts

**Discovery** runs at startup and on demand: Codex from `~/.codex/auth.json`,
Claude from the macOS Keychain plus `~/.claude.json` for the identity. Anything
found is merged into subbier's own store; an account already there keeps
subbier's fresher tokens. Only *additional* accounts need the browser login.

**Identity** is `provider:account_id`, the stable key that config and history
reference. Frontends see a per-process integer id instead.

**Credentials** live in `~/.subbier/subs.json`, mode 0600, written atomically.
Never in config, so config is safe to paste into an issue. Token refresh is
deduplicated per account and refreshes early; a permanent refresh failure
marks the account `needs login`, a transient one does not.

**Labels** are the address as the vendor spelled it, disambiguated only when
two accounts of one provider would read the same.

## 4. Providers

Codex and Claude differ in two ways, handled two ways:

- Differences that are **parameters** (client id, URLs, token body shape,
  how expiry is expressed, how `state` is derived) are a static table. One
  OAuth and PKCE implementation reads it. Nothing in the login flow branches
  on provider.
- Differences that are **algorithms** (parsing usage, the proxy's request
  shape) are two modules dispatched by one `match`. There are two providers
  and there will not be a tenth, so there is no trait object.

Everything provider-specific dies at the parse boundary. `Usage` is
provider-free: a session window, a weekly window, any scoped windows, and the
provider's own verdict on whether the account is cut off. That verdict beats
the percentage, because enforcement can lead the number.

The two **proxy paths are not unified**. The Codex path forces streaming,
rewrites roles, strips cache keys, emulates conversation state from a local
transcript store, and reassembles SSE into JSON. The Anthropic path is a
stateless forward. They share SSE framing, header scrubbing, and the balancer,
and nothing else.

## 5. Routing

Every request goes through the same selection:

```
filter   provider matches · enabled · proxied · not exhausted · not needs-login
         · in the pool (if any) · under the pool's ceilings (if any)
then     pin        the user forced this account
         frozen     auto-switch is off and the current account still works
         affinity   the request continues a Codex conversation placed here
         placement  its prompt_cache_key was last sent here
         sticky     the current account is still a candidate
         strategy   lowest usage · highest usage · round robin · least connections
```

Hints are soft: one that names an account no longer eligible falls through.
Stickiness short-circuits before any usage fetch, so "lowest usage" is
consulted only at rotation time. The usage strategies rank on the account-wide
allowance and so see out-of-band traffic; least-connections sees only proxy
connections, which is the most it can balance.

**Auto-switch** is the difference between a balancer and a plain proxy. On, a
request that hits an exhausted or dead account rotates and retries. Off, it
selects once and never changes identity mid-request.

**Exhaustion** is in memory only. An account confirmed at 100 percent is
skipped until its window resets; a failed usage fetch ranks last but is never
quarantined, because "unknown" is not "full".

**Failures are named**, because collapsing them burns every account on one bad
request. A 429 is classified by its body: a usage-limit body rotates and
quarantines, any other 429 passes through for the client to back off. A
permanent token failure rotates; a transient one returns 502 and rotates
nothing. Anything else passes through untouched.

A **transport** failure carries no status at all and so says nothing about the
account: the request is resent to the *same* one — twice if no response ever
started, once more if one died before the client had a byte. A stream already
being read cannot be resent and is recorded as the 502 it became. Upstream
calls go over HTTP/1.1 for the same reason: one connection per in-flight
request, so a connection the far end tears down costs one request rather than
every request sharing it.

## 6. Where state lives

> Config is user intent. sqlite is time series. Memory is everything derivable.
> Secrets get their own file.

- **`~/.subbier/config.kdl`** is optional and hand-editable. Every menu
  control maps to one key and back. Write-back edits the document in place so
  comments and ordering survive a click. A missing file is the defaults; an
  unknown key is a warning. The engine watches the file, so a saved pool
  appears without a restart.
- **`~/.subbier/subs.json`** is the credential store (section 3).
- **`~/.subbier/state.db`** holds two time series and nothing else: allowance
  samples per poll, and one row per proxied request. Separate tables so the
  two kinds of number cannot be summed by accident. Pruned to the retention
  window daily.
- **`~/.subbier/transcripts.db`** is the Codex conversation store behind
  `previous_response_id` emulation, plus where each cache key was last placed.
  It is derived data, evicted by age and size, and losing it costs only the
  conversations in flight.
- **Memory** holds the usage cache and its per-account backoff, exhaustion,
  the current and pinned account, in-flight gauges, the recent-token ring,
  and the model catalog. A restart re-probes reality rather than resurrecting
  an opinion about it.

## 7. Presenting usage

- **Severity** is `Ok`, `Warn`, `Critical` against two configurable
  thresholds. It is computed once, in the engine, so every frontend agrees.
- **Notifications** fire only on an upward band crossing, once. A process
  that starts while usage is already high did not observe a crossing.
- **The projection** is a straight line from the window's start to now,
  extended to 100 percent. It is shown only when it lands *before* the reset;
  past the reset it is not actionable and is withheld. It is a readout. The
  router never acts on it.
- **The overall percentage** in the menu bar is a plan-weighted mean of each
  enabled account's worst window, computed by the engine so the menu and
  `subbier status --json` cannot disagree.
- **History** keeps the two series apart one level up from the schema: they
  are different types and cannot share an axis. They also disagree about what
  an empty bucket means. No allowance sample means nobody polled, so the last
  value is held briefly and then drawn as a break. No proxied rows means no
  traffic, which is a real zero. Percentages are always drawn against a fixed
  0 to 100 axis so a flat 3 and a flat 95 never look the same.

## 8. The proxy

One port serves both providers. Codex clients point `OPENAI_BASE_URL` at
`/v1`; Anthropic clients point `ANTHROPIC_BASE_URL` at the root. Explicit
`/codex/...` and `/anthropic/...` prefixes exist for the one path both define.
Every route is also served under `/pool/<name>/`, narrowed to that pool, so a
pool is just a base URL.

`GET /status` returns the snapshot as JSON. That is how `subbier status` and
`subbier watch` work against a running instance, how a second process learns
one is already there, and what a future frontend on another platform would
read.

Client headers are dropped and rebuilt per attempt. Response encoding headers
are stripped because the body has already been decoded. If a proxy key is
configured it is required; without one, only loopback binds are allowed.

## 9. The menu bar mark

The icon is `sR`, Rust's `.rs` read backwards, drawn on an 18-pixel grid so
every edge lands on a whole pixel at 1x, 2x and 3x. It is a macOS template
image: monochrome, auto-inverted for dark mode, and therefore unable to carry
colour. Severity rides on the text beside it, a percentage with a `!` when
anything is critical. The icon is embedded in the binary so subbier runs
without an `.app` bundle.

## 10. Not built, on purpose

- Cost in dollars. For subscription accounts it is misleading and the price
  table is a permanent tax.
- Reconciling allowance against proxied tokens. The gap is information.
- Routing on a projection, on cost, or on model. Rotating on a guess moves
  traffic on a guess.
- Log scraping of CLI session files.
- Writing tokens back to the CLIs' own stores, or storing subbier's in the
  Keychain.
- More providers, or a plugin system for them.
- A Linux frontend, multi-machine sync, teams, remote access, auto-update.
- Anything that turns the menu into a dashboard. `subbier watch` has the room
  for charts; the menu stays a readout.
