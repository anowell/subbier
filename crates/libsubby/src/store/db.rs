//! `~/.subbier/state.db` (sqlite, WAL) — time series only. `allowance_sample`
//! holds account-wide percentages; `proxied_request` holds one row per request
//! subbier routed, a subset. The names are what keep the two from being blended.
//! One thread owns the `Connection`; a full queue drops the row, never stalls.

use std::path::Path;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::time::{Duration, Instant};

use jiff::{SignedDuration, Timestamp};
use rusqlite::Connection;
use tokio::sync::oneshot;

use crate::error::{Error, Result};
use crate::model::{Provider, SubKey, WindowKind};

/// `ts` columns are unix **seconds**; rusqlite's `jiff` feature reads exactly
/// that integer encoding back.
const SCHEMA: &str = "\
-- ACCOUNT-WIDE: includes traffic that never touched subbier.
CREATE TABLE IF NOT EXISTS allowance_sample (
  ts INTEGER NOT NULL,
  sub_key TEXT NOT NULL,
  window TEXT NOT NULL,
  pct REAL NOT NULL,
  resets_at INTEGER
);
CREATE INDEX IF NOT EXISTS allowance_sample_ts ON allowance_sample(sub_key, ts);

-- PROXY-OBSERVED ONLY: a strict subset of what moved allowance_sample.pct.
CREATE TABLE IF NOT EXISTS proxied_request (
  ts INTEGER NOT NULL,
  sub_key TEXT NOT NULL,
  provider TEXT NOT NULL,
  route TEXT NOT NULL,
  status INTEGER NOT NULL,
  duration_ms INTEGER NOT NULL,
  input_tokens INTEGER,
  output_tokens INTEGER
);
CREATE INDEX IF NOT EXISTS proxied_request_ts ON proxied_request(ts);
";

/// WAL so a reader never blocks the writer; `NORMAL` because losing the last
/// few metric rows to a power cut is not worth an fsync per insert.
const PRAGMAS: &str = "\
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
PRAGMA busy_timeout=5000;
";

const QUEUE_CAPACITY: usize = 1024;

/// How often the writer thread wakes to consider pruning.
const TICK: Duration = Duration::from_secs(60 * 60);

const PRUNE_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

const DAY_SECONDS: i64 = 24 * 60 * 60;

/// One row of `proxied_request`: proxy-observed only, never account usage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxiedRequestRow {
    /// When the request completed.
    pub ts: Timestamp,
    pub sub: SubKey,
    pub provider: Provider,
    pub route: String,
    /// The status subbier returned to the client, not necessarily upstream's.
    pub status: u16,
    pub duration_ms: u32,
    /// Tokens the *proxy* observed, when the response reported any.
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
}

/// One `allowance_sample` row. ACCOUNT-WIDE: never plot it on the same axis as
/// a [`ProxiedBucket`].
#[derive(Debug, Clone, PartialEq)]
pub struct AllowanceRow {
    pub ts: Timestamp,
    pub sub: SubKey,
    pub window: WindowKind,
    pub pct: f32,
}

/// `proxied_request` rows for one route, summed over one time bucket.
/// PROXY-OBSERVED ONLY.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxiedBucket {
    /// The bucket's start, floored to a multiple of the requested width.
    pub ts: Timestamp,
    /// The path the request came in on, `/pool/<name>/` prefix included.
    pub route: String,
    pub requests: u32,
    /// Input plus output tokens observed; a row that reported none adds zero.
    pub tokens: u64,
}

/// A handle to the sqlite writer thread. Dropping the last one stops the thread
/// and closes the connection.
#[derive(Debug)]
pub struct Db {
    tx: SyncSender<DbOp>,
}

enum DbOp {
    Allowance {
        ts: Timestamp,
        sub: SubKey,
        window: WindowKind,
        pct: f32,
        resets_at: Option<Timestamp>,
    },
    Proxied(Box<ProxiedRequestRow>),
    AllowanceSince {
        since: Timestamp,
        reply: oneshot::Sender<Result<Vec<AllowanceRow>>>,
    },
    ProxiedSince {
        since: Timestamp,
        bucket_secs: i64,
        reply: oneshot::Sender<Result<Vec<ProxiedBucket>>>,
    },
}

impl Db {
    /// Open (or create) `path`, prune anything older than `retain_days`, and
    /// start the writer thread.
    pub fn open(path: &Path, retain_days: u32) -> Result<Db> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            super::ensure_dir(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(PRAGMAS)?;
        conn.execute_batch(SCHEMA)?;
        prune(&conn, retain_days)?;

        let (tx, rx) = sync_channel(QUEUE_CAPACITY);
        std::thread::Builder::new()
            .name("subbier-sqlite".into())
            .spawn(move || writer_loop(conn, rx, retain_days))?;
        Ok(Db { tx })
    }

    /// Record one **account-wide** allowance percentage. Fire-and-forget.
    pub fn record_allowance(
        &self,
        sub: &SubKey,
        window: &WindowKind,
        pct: f32,
        resets_at: Option<Timestamp>,
    ) {
        self.offer(DbOp::Allowance {
            ts: Timestamp::now(),
            sub: sub.clone(),
            window: window.clone(),
            pct,
            resets_at,
        });
    }

    /// Record one request **the proxy routed**. Fire-and-forget.
    pub fn record_proxied_request(&self, row: ProxiedRequestRow) {
        self.offer(DbOp::Proxied(Box::new(row)));
    }

    /// Every allowance sample since `since`, for every sub and window, oldest
    /// first. Group them with [`crate::history::AllowanceHistory`].
    ///
    /// Travels the same channel as the writes, so it doubles as a barrier:
    /// every sample handed over before this call has landed when it returns.
    pub async fn allowance_since(&self, since: Timestamp) -> Result<Vec<AllowanceRow>> {
        let (reply, answer) = oneshot::channel();
        self.ask(DbOp::AllowanceSince { since, reply }, answer)
            .await
    }

    /// Proxied requests since `since`, summed into `bucket`-wide buckets per
    /// route, oldest first. A bucket with no traffic produces **no row**: only
    /// the caller knows whether that is a gap or a floor.
    ///
    /// A non-positive `bucket` is an error.
    pub async fn proxied_since(
        &self,
        since: Timestamp,
        bucket: SignedDuration,
    ) -> Result<Vec<ProxiedBucket>> {
        let bucket_secs = bucket.as_secs();
        if bucket_secs <= 0 {
            return Err(Error::other("a proxied-request bucket must be positive"));
        }
        let (reply, answer) = oneshot::channel();
        self.ask(
            DbOp::ProxiedSince {
                since,
                bucket_secs,
                reply,
            },
            answer,
        )
        .await
    }

    async fn ask<T>(&self, op: DbOp, answer: oneshot::Receiver<Result<T>>) -> Result<T> {
        self.tx.try_send(op).map_err(|e| match e {
            TrySendError::Full(_) => Error::other("state.db queue is full"),
            TrySendError::Disconnected(_) => Error::other("state.db writer thread has stopped"),
        })?;
        answer
            .await
            .map_err(|_| Error::other("state.db writer thread stopped before replying"))?
    }

    /// Hand an operation to the writer thread, or drop it with a `warn`.
    fn offer(&self, op: DbOp) {
        match self.tx.try_send(op) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                tracing::warn!("state.db write queue is full; dropping a metric row");
            }
            Err(TrySendError::Disconnected(_)) => {
                tracing::warn!("state.db writer thread has stopped; dropping a metric row");
            }
        }
    }
}

// The engine shares one `Db` across the poller, the proxy and the command loop.
const _: fn() = || {
    fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<Db>();
};

/// The one thread that ever touches the `Connection`.
fn writer_loop(conn: Connection, rx: Receiver<DbOp>, retain_days: u32) {
    let mut last_prune = Instant::now();
    loop {
        match rx.recv_timeout(TICK) {
            Ok(op) => apply(&conn, op),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if last_prune.elapsed() >= PRUNE_EVERY {
            last_prune = Instant::now();
            if let Err(e) = prune(&conn, retain_days) {
                tracing::warn!(error = %e, "state.db daily prune failed");
            }
        }
    }
}

fn apply(conn: &Connection, op: DbOp) {
    match op {
        DbOp::Allowance {
            ts,
            sub,
            window,
            pct,
            resets_at,
        } => {
            let result = conn.execute(
                "INSERT INTO allowance_sample (ts, sub_key, window, pct, resets_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    ts.as_second(),
                    sub.as_str(),
                    window.as_str(),
                    pct,
                    resets_at.map(Timestamp::as_second),
                ],
            );
            if let Err(e) = result {
                tracing::warn!(error = %e, "dropping an allowance sample");
            }
        }
        DbOp::Proxied(row) => {
            let result = conn.execute(
                "INSERT INTO proxied_request \
                 (ts, sub_key, provider, route, status, duration_ms, input_tokens, output_tokens) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    row.ts.as_second(),
                    row.sub.as_str(),
                    row.provider.id(),
                    row.route,
                    row.status,
                    row.duration_ms,
                    row.input_tokens,
                    row.output_tokens,
                ],
            );
            if let Err(e) = result {
                tracing::warn!(error = %e, "dropping a proxied-request row");
            }
        }
        DbOp::AllowanceSince { since, reply } => {
            let _ = reply.send(query_allowance_since(conn, since));
        }
        DbOp::ProxiedSince {
            since,
            bucket_secs,
            reply,
        } => {
            let _ = reply.send(query_proxied_since(conn, since, bucket_secs));
        }
    }
}

fn prune(conn: &Connection, retain_days: u32) -> Result<()> {
    let cutoff = Timestamp::now().as_second() - i64::from(retain_days) * DAY_SECONDS;
    conn.execute("DELETE FROM allowance_sample WHERE ts < ?1", [cutoff])?;
    conn.execute("DELETE FROM proxied_request WHERE ts < ?1", [cutoff])?;
    Ok(())
}

fn query_allowance_since(conn: &Connection, since: Timestamp) -> Result<Vec<AllowanceRow>> {
    let mut stmt = conn.prepare(
        "SELECT ts, sub_key, window, pct FROM allowance_sample \
         WHERE ts >= ?1 ORDER BY ts",
    )?;
    let rows = stmt.query_map([since.as_second()], |row| {
        Ok(AllowanceRow {
            ts: row.get(0)?,
            sub: SubKey::from(row.get::<_, String>(1)?),
            window: WindowKind::from(row.get::<_, String>(2)?),
            pct: row.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn query_proxied_since(
    conn: &Connection,
    since: Timestamp,
    bucket_secs: i64,
) -> Result<Vec<ProxiedBucket>> {
    // COALESCE, not a filter: a request that reported no usage still counts.
    let mut stmt = conn.prepare(
        "SELECT (ts / ?2) * ?2 AS bucket, route, COUNT(*), \
                SUM(COALESCE(input_tokens, 0) + COALESCE(output_tokens, 0)) \
         FROM proxied_request WHERE ts >= ?1 \
         GROUP BY bucket, route ORDER BY bucket",
    )?;
    let rows = stmt.query_map(rusqlite::params![since.as_second(), bucket_secs], |row| {
        Ok(ProxiedBucket {
            ts: row.get(0)?,
            route: row.get(1)?,
            requests: row.get::<_, i64>(2)?.try_into().unwrap_or(u32::MAX),
            tokens: row.get::<_, i64>(3)?.try_into().unwrap_or(0),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests_support::temp_dir;
    use jiff::SignedDuration;

    fn key() -> SubKey {
        SubKey::new(Provider::Codex, "acct-1")
    }

    fn row_at(ts: Timestamp) -> ProxiedRequestRow {
        ProxiedRequestRow {
            ts,
            sub: key(),
            provider: Provider::Codex,
            route: "/v1/responses".into(),
            status: 200,
            duration_ms: 1234,
            input_tokens: Some(10),
            output_tokens: Some(20),
        }
    }

    /// The public writer always stamps `Timestamp::now()`, so backdated rows
    /// have to be inserted directly.
    fn backdate(path: &Path, ts: Timestamp) {
        let conn = Connection::open(path).unwrap();
        conn.execute(
            "INSERT INTO allowance_sample (ts, sub_key, window, pct, resets_at) \
             VALUES (?1, ?2, 'session', 1.0, NULL)",
            rusqlite::params![ts.as_second(), key().as_str()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO proxied_request \
             (ts, sub_key, provider, route, status, duration_ms, input_tokens, output_tokens) \
             VALUES (?1, ?2, 'codex', '/v1/responses', 200, 1, 1, 1)",
            rusqlite::params![ts.as_second(), key().as_str()],
        )
        .unwrap();
    }

    #[tokio::test]
    async fn allowance_samples_round_trip_with_their_window_intact() {
        let dir = temp_dir("db-allowance");
        let db = Db::open(&dir.join("state.db"), 7).unwrap();

        db.record_allowance(&key(), &WindowKind::Session, 12.5, None);
        db.record_allowance(
            &key(),
            &WindowKind::Weekly,
            40.0,
            Some(Timestamp::from_second(1_700_000_000).unwrap()),
        );
        db.record_allowance(&key(), &WindowKind::Scoped("fable".into()), 3.0, None);

        let since = Timestamp::now() - SignedDuration::from_secs(60);
        let rows = db.allowance_since(since).await.unwrap();
        let windows: Vec<_> = rows.iter().map(|r| (r.window.clone(), r.pct)).collect();
        assert_eq!(
            windows,
            vec![
                (WindowKind::Session, 12.5),
                (WindowKind::Weekly, 40.0),
                (WindowKind::Scoped("fable".into()), 3.0),
            ]
        );
        assert!(rows.iter().all(|r| r.sub == key()));

        assert!(
            db.allowance_since(Timestamp::now() + SignedDuration::from_secs(60))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn proxied_requests_bucket_by_route() {
        let dir = temp_dir("db-proxied");
        let db = Db::open(&dir.join("state.db"), 7).unwrap();

        let now = Timestamp::now();
        db.record_proxied_request(row_at(now));
        db.record_proxied_request(ProxiedRequestRow {
            output_tokens: None,
            ..row_at(now)
        });

        let buckets = db
            .proxied_since(
                now - SignedDuration::from_secs(60),
                SignedDuration::from_hours(1),
            )
            .await
            .unwrap();
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].route, "/v1/responses");
        assert_eq!(
            buckets[0].requests, 2,
            "a request with no usage still counts"
        );
        assert_eq!(buckets[0].tokens, 40, "10 + 20, then 10 + nothing");

        assert!(db.proxied_since(now, SignedDuration::ZERO).await.is_err());
    }

    #[tokio::test]
    async fn opening_prunes_rows_older_than_the_retention_window() {
        let dir = temp_dir("db-prune");
        let path = dir.join("state.db");

        let db = Db::open(&path, 7).unwrap();
        let now = Timestamp::now();
        backdate(&path, now - SignedDuration::from_hours(24 * 30));
        db.record_allowance(&key(), &WindowKind::Session, 50.0, None);

        let all = now - SignedDuration::from_hours(24 * 365);
        assert_eq!(db.allowance_since(all).await.unwrap().len(), 2);
        drop(db);

        let db = Db::open(&path, 7).unwrap();
        let kept = db.allowance_since(all).await.unwrap();
        assert_eq!(kept.len(), 1, "the 30-day-old sample should be gone");
        assert_eq!(
            kept[0].pct, 50.0,
            "and the fresh one should survive a reopen"
        );

        let conn = Connection::open(&path).unwrap();
        let proxied: i64 = conn
            .query_row("SELECT COUNT(*) FROM proxied_request", [], |r| r.get(0))
            .unwrap();
        assert_eq!(proxied, 0, "prune must cover proxied_request too");
    }
}
