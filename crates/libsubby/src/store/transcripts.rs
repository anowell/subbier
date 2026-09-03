//! `~/.subbier/transcripts.db` (sqlite, WAL): `previous_response_id` emulated,
//! since the Codex backend rejects `store: true`. A row holds only its own turn,
//! and [`TranscriptStore::chain`] restamps every ancestor so eviction takes dead
//! conversations first. One `Mutex<Connection>`: the request path is synchronous.

use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use crate::error::Result;
use crate::model::SubKey;

/// `created_at`/`touched_at` are unix **seconds**, as in [`super::db`].
/// `sub_key` is part of the turn: a chain replayed against a different account
/// is usually rejected upstream.
const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS response (
  id         TEXT PRIMARY KEY,
  parent_id  TEXT,
  sub_key    TEXT NOT NULL,
  input      TEXT NOT NULL,
  output     TEXT NOT NULL,
  bytes      INTEGER NOT NULL,
  created_at INTEGER NOT NULL,
  touched_at INTEGER NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS response_touched_at ON response(touched_at);
CREATE INDEX IF NOT EXISTS response_parent_id ON response(parent_id);
CREATE TABLE IF NOT EXISTS placement (
  key        TEXT PRIMARY KEY,
  sub_key    TEXT NOT NULL,
  touched_at INTEGER NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS placement_touched_at ON placement(touched_at);
";

/// WAL so a chain read never blocks a turn being written; `NORMAL` because a
/// power cut losing the last turn is a 400 on one conversation.
const PRAGMAS: &str = "\
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
PRAGMA busy_timeout=5000;
";

/// How far a walk goes before the chain is called broken; a bound, not a
/// policy, so a `parent_id` cycle cannot hang a request thread.
const MAX_DEPTH: i64 = 10_000;

/// When a remembered turn stops being worth its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Turns only; placements are never evicted for space.
    pub max_bytes: u64,
    /// Measured from the last time the row was *used*, not written.
    pub ttl: Duration,
}

impl Default for Limits {
    /// A gigabyte and a day: past any live conversation, unremarkable on disk.
    fn default() -> Self {
        Self {
            max_bytes: 1024 * 1024 * 1024,
            ttl: Duration::from_secs(24 * 60 * 60),
        }
    }
}

/// One turn as stored: the items **this** turn added, never the chain it hangs
/// off.
#[derive(Debug, Clone, PartialEq)]
pub struct Turn {
    /// The upstream response id, which is what the next turn will name.
    pub id: String,
    /// `None` for a root.
    pub parent: Option<String>,
    /// The account that served it, and so the affinity hint for its children.
    pub sub: SubKey,
    /// What the client sent *this* turn, before any splicing.
    pub input: Vec<Value>,
    pub output: Vec<Value>,
}

/// A resolved conversation: what to splice into the next request's `input`.
#[derive(Debug, Clone, PartialEq)]
pub struct Chain {
    /// `[root.input…, root.output…, …, head.input…, head.output…]`.
    pub items: Vec<Value>,
    /// The sub that served the head — the affinity hint for the next turn.
    pub sub: SubKey,
}

/// The connection and the byte total eviction runs against, under one mutex:
/// a total that could drift from the rows it counts is worse than the lock.
#[derive(Debug)]
struct Inner {
    conn: Connection,
    bytes: u64,
}

/// The `previous_response_id` store. Every method takes `&self` and blocks.
#[derive(Debug)]
pub struct TranscriptStore {
    inner: Mutex<Inner>,
    limits: Limits,
}

impl TranscriptStore {
    /// Open (or create) `path`, with the directory 0700 and the file 0600.
    pub fn open(path: &Path, limits: Limits) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            super::ensure_dir(parent)?;
        }
        let conn = Connection::open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            // Before the first write: sqlite copies this mode onto the `-wal`
            // and `-shm` files it creates beside the database.
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(super::FILE_MODE))?;
        }
        Self::start(conn, limits)
    }

    /// A store that dies with the process: what tests use, and what the engine
    /// falls back to when the real file cannot be opened.
    pub fn in_memory(limits: Limits) -> Result<Self> {
        Self::start(Connection::open_in_memory()?, limits)
    }

    fn start(conn: Connection, limits: Limits) -> Result<Self> {
        conn.execute_batch(PRAGMAS)?;
        conn.execute_batch(SCHEMA)?;
        let bytes = stored_bytes(&conn)?;
        Ok(Self {
            inner: Mutex::new(Inner { conn, bytes }),
            limits,
        })
    }

    /// Write a turn down, replacing any turn already under that id, then
    /// evict. A turn bigger than [`Limits::max_bytes`] is skipped rather than
    /// emptying the store for something that still would not fit.
    pub fn remember(&self, turn: Turn) -> Result<()> {
        self.remember_at(turn, Timestamp::now())
    }

    /// The whole conversation ending at `head`, oldest-first, touching every
    /// row in it. `None` if it cannot be replayed whole — an unknown head or an
    /// evicted ancestor — because a hole is never quietly stitched over.
    pub fn chain(&self, head: &str) -> Result<Option<Chain>> {
        self.chain_at(head, Timestamp::now())
    }

    /// The sub `key` was last placed on, touching the row so a key still in use
    /// does not expire under the conversation using it.
    pub fn placement(&self, key: &str) -> Result<Option<SubKey>> {
        self.placement_at(key, Timestamp::now())
    }

    /// Place `key` on `sub`, replacing any earlier placement.
    pub fn place(&self, key: &str, sub: &SubKey) -> Result<()> {
        self.place_at(key, sub, Timestamp::now())
    }

    pub fn len(&self) -> Result<u64> {
        let inner = self.lock();
        let count: i64 = inner
            .conn
            .query_row("SELECT COUNT(*) FROM response", [], |row| row.get(0))?;
        Ok(count.unsigned_abs())
    }

    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Bytes held, re-read from the rows rather than from the running total
    /// eviction uses, so asserting on it also catches that total drifting.
    pub fn bytes(&self) -> Result<u64> {
        let inner = self.lock();
        stored_bytes(&inner.conn)
    }

    pub fn contains(&self, id: &str) -> Result<bool> {
        let inner = self.lock();
        Ok(inner
            .conn
            .query_row("SELECT 1 FROM response WHERE id = ?1", [id], |_| Ok(()))
            .optional()?
            .is_some())
    }

    fn remember_at(&self, turn: Turn, now: Timestamp) -> Result<()> {
        let input = serde_json::to_string(&turn.input)?;
        let output = serde_json::to_string(&turn.output)?;
        let bytes = (input.len() + output.len()) as u64;
        if bytes > self.limits.max_bytes {
            tracing::warn!(
                id = %turn.id,
                bytes,
                max_bytes = self.limits.max_bytes,
                "a turn larger than the whole transcript store is not remembered"
            );
            return Ok(());
        }

        let mut guard = self.lock();
        let inner = &mut *guard;
        let replaced: Option<i64> = inner
            .conn
            .query_row(
                "SELECT bytes FROM response WHERE id = ?1",
                [&turn.id],
                |row| row.get(0),
            )
            .optional()?;
        inner.conn.execute(
            "INSERT OR REPLACE INTO response \
             (id, parent_id, sub_key, input, output, bytes, created_at, touched_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
            params![
                turn.id,
                turn.parent,
                turn.sub.as_str(),
                input,
                output,
                bytes as i64,
                now.as_second(),
            ],
        )?;
        inner.bytes = inner.bytes + bytes - replaced.unwrap_or(0).unsigned_abs();
        evict(inner, &self.limits, now)
    }

    fn chain_at(&self, head: &str, now: Timestamp) -> Result<Option<Chain>> {
        let guard = self.lock();
        let links = walk(&guard.conn, head)?;
        let Some(chain) = assemble(&links)? else {
            return Ok(None);
        };
        touch(&guard.conn, &links, now)?;
        Ok(Some(chain))
    }

    fn placement_at(&self, key: &str, now: Timestamp) -> Result<Option<SubKey>> {
        let inner = self.lock();
        // Read against the cutoff rather than trusting the sweep: eviction only
        // runs when a turn is written, which keyed traffic alone never does.
        let placed: Option<String> = inner
            .conn
            .query_row(
                "SELECT sub_key FROM placement WHERE key = ?1 AND touched_at >= ?2",
                params![key, cutoff(&self.limits, now)],
                |row| row.get(0),
            )
            .optional()?;
        if placed.is_some() {
            inner.conn.execute(
                "UPDATE placement SET touched_at = ?1 WHERE key = ?2",
                params![now.as_second(), key],
            )?;
        }
        Ok(placed.map(SubKey::from))
    }

    fn place_at(&self, key: &str, sub: &SubKey, now: Timestamp) -> Result<()> {
        let inner = self.lock();
        inner.conn.execute(
            "INSERT OR REPLACE INTO placement (key, sub_key, touched_at) VALUES (?1, ?2, ?3)",
            params![key, sub.as_str(), now.as_second()],
        )?;
        Ok(())
    }

    /// A poisoned lock means a panic *while holding it*, which here can only
    /// have left sqlite mid-statement, not the byte total mid-update.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

struct Link {
    id: String,
    parent: Option<String>,
    sub: String,
    input: String,
    output: String,
}

/// `head` and its ancestors, head first, depth-bounded by [`MAX_DEPTH`].
fn walk(conn: &Connection, head: &str) -> Result<Vec<Link>> {
    let mut stmt = conn.prepare(
        "WITH RECURSIVE ancestry(id, parent_id, sub_key, input, output, depth) AS ( \
             SELECT id, parent_id, sub_key, input, output, 0 \
               FROM response WHERE id = ?1 \
             UNION ALL \
             SELECT r.id, r.parent_id, r.sub_key, r.input, r.output, a.depth + 1 \
               FROM response r JOIN ancestry a ON r.id = a.parent_id \
              WHERE a.depth < ?2 \
         ) \
         SELECT id, parent_id, sub_key, input, output FROM ancestry ORDER BY depth",
    )?;
    let rows = stmt.query_map(params![head, MAX_DEPTH], |row| {
        Ok(Link {
            id: row.get(0)?,
            parent: row.get(1)?,
            sub: row.get(2)?,
            input: row.get(3)?,
            output: row.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Flatten a walk oldest-first, or `None` if it is not a whole conversation:
/// empty, missing an ancestor, or still climbing at [`MAX_DEPTH`] — a cycle
/// looks the same as depth, and both mean "do not replay this".
fn assemble(links: &[Link]) -> Result<Option<Chain>> {
    let Some(head) = links.first() else {
        return Ok(None);
    };
    let intact = links
        .windows(2)
        .all(|pair| pair[0].parent.as_deref() == Some(pair[1].id.as_str()));
    let rooted = links.last().is_some_and(|root| root.parent.is_none());
    if !intact || !rooted {
        return Ok(None);
    }

    let mut items = Vec::new();
    for link in links.iter().rev() {
        items.extend(serde_json::from_str::<Vec<Value>>(&link.input)?);
        items.extend(serde_json::from_str::<Vec<Value>>(&link.output)?);
    }
    Ok(Some(Chain {
        items,
        sub: SubKey::from(head.sub.clone()),
    }))
}

/// Restamp the whole chain, not just the head: that is what makes eviction by
/// least-recently-touched take dead conversations rather than live roots.
fn touch(conn: &Connection, links: &[Link], now: Timestamp) -> Result<()> {
    let mut stmt = conn.prepare("UPDATE response SET touched_at = ?1 WHERE id = ?2")?;
    for link in links {
        stmt.execute(params![now.as_second(), link.id])?;
    }
    Ok(())
}

fn stored_bytes(conn: &Connection) -> Result<u64> {
    let sum: i64 = conn.query_row("SELECT COALESCE(SUM(bytes), 0) FROM response", [], |row| {
        row.get(0)
    })?;
    Ok(sum.unsigned_abs())
}

/// The oldest `touched_at` still inside the ttl.
fn cutoff(limits: &Limits, now: Timestamp) -> i64 {
    let ttl = i64::try_from(limits.ttl.as_secs()).unwrap_or(i64::MAX);
    now.as_second().saturating_sub(ttl)
}

/// Expiry first — turns and placements share the clock — then
/// least-recently-touched turns until the byte cap is met.
fn evict(inner: &mut Inner, limits: &Limits, now: Timestamp) -> Result<()> {
    let cutoff = cutoff(limits, now);
    inner
        .conn
        .execute("DELETE FROM placement WHERE touched_at < ?1", [cutoff])?;
    let expired: i64 = inner.conn.query_row(
        "SELECT COALESCE(SUM(bytes), 0) FROM response WHERE touched_at < ?1",
        [cutoff],
        |row| row.get(0),
    )?;
    if expired > 0 {
        inner
            .conn
            .execute("DELETE FROM response WHERE touched_at < ?1", [cutoff])?;
        inner.bytes = inner.bytes.saturating_sub(expired.unsigned_abs());
    }

    while inner.bytes > limits.max_bytes {
        let victim: Option<(String, i64)> = inner
            .conn
            .query_row(
                "SELECT id, bytes FROM response ORDER BY touched_at, rowid LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((id, bytes)) = victim else { break };
        inner
            .conn
            .execute("DELETE FROM response WHERE id = ?1", [&id])?;
        inner.bytes = inner.bytes.saturating_sub(bytes.unsigned_abs());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Provider;
    use crate::store::tests_support::temp_dir;
    use serde_json::json;

    fn key() -> SubKey {
        SubKey::new(Provider::Codex, "acct-1")
    }

    fn store() -> TranscriptStore {
        TranscriptStore::in_memory(Limits::default()).unwrap()
    }

    /// Every fixture turn costs the same bytes, so a cap can be written in turns.
    fn turn(id: &str, parent: Option<&str>) -> Turn {
        let padded = |kind: &str| format!("{:<16}", format!("{kind} {id}"));
        Turn {
            id: id.to_owned(),
            parent: parent.map(str::to_owned),
            sub: key(),
            input: vec![json!({ "role": "user", "content": padded("in") })],
            output: vec![json!({ "type": "message", "content": padded("out") })],
        }
    }

    fn contents(chain: &Chain) -> Vec<String> {
        chain
            .items
            .iter()
            .map(|item| {
                item["content"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .to_owned()
            })
            .collect()
    }

    fn at(secs: i64) -> Timestamp {
        Timestamp::from_second(secs).unwrap()
    }

    fn unit() -> u64 {
        let probe = store();
        probe.remember(turn("probe", None)).unwrap();
        probe.bytes().unwrap()
    }

    fn capped(turns: u64) -> TranscriptStore {
        TranscriptStore::in_memory(Limits {
            max_bytes: unit() * turns,
            ttl: Duration::from_secs(3600),
        })
        .unwrap()
    }

    #[test]
    fn a_chain_assembles_every_ancestor_oldest_first() {
        let store = store();
        store.remember(turn("r1", None)).unwrap();
        store.remember(turn("r2", Some("r1"))).unwrap();
        store.remember(turn("r3", Some("r2"))).unwrap();

        let chain = store.chain("r3").unwrap().expect("whole chain");
        assert_eq!(
            contents(&chain),
            vec!["in r1", "out r1", "in r2", "out r2", "in r3", "out r3"],
            "input then output, per turn, oldest first"
        );
        assert_eq!(chain.sub, key(), "the head's sub is the affinity hint");

        // A mid-chain id resolves to its own prefix, so branching works.
        let chain = store.chain("r2").unwrap().unwrap();
        assert_eq!(contents(&chain), vec!["in r1", "out r1", "in r2", "out r2"]);
        assert!(store.chain("r9").unwrap().is_none(), "unknown head");
    }

    #[test]
    fn storage_is_linear_in_the_number_of_turns() {
        let store = store();
        store.remember(turn("r1", None)).unwrap();
        let one = store.bytes().unwrap();
        store.remember(turn("r2", Some("r1"))).unwrap();
        store.remember(turn("r3", Some("r2"))).unwrap();

        let three = store.bytes().unwrap();
        assert_eq!(store.len().unwrap(), 3);
        assert!(
            three < one * 4,
            "three turns of {one} bytes each should cost about {}, not {three}",
            one * 3
        );
    }

    #[test]
    fn reading_a_chain_touches_its_ancestors_so_dead_chains_are_evicted_first() {
        let store = capped(4);
        store.remember_at(turn("live1", None), at(1_000)).unwrap();
        store
            .remember_at(turn("live2", Some("live1")), at(1_000))
            .unwrap();
        store.remember_at(turn("dead1", None), at(1_001)).unwrap();
        store
            .remember_at(turn("dead2", Some("dead1")), at(1_001))
            .unwrap();
        assert_eq!(store.len().unwrap(), 4);

        store.chain_at("live2", at(2_000)).unwrap().unwrap();
        store.remember_at(turn("other1", None), at(2_001)).unwrap();
        store.remember_at(turn("other2", None), at(2_002)).unwrap();

        assert!(
            store.contains("live1").unwrap(),
            "the live chain's ROOT must survive, not just its head"
        );
        assert!(store.contains("live2").unwrap());
        assert!(!store.contains("dead1").unwrap(), "the dead chain goes");
        assert!(!store.contains("dead2").unwrap());
        assert_eq!(store.len().unwrap(), 4);
    }

    #[test]
    fn a_chain_untouched_for_the_ttl_expires() {
        let store = TranscriptStore::in_memory(Limits {
            max_bytes: u64::MAX,
            ttl: Duration::from_secs(60),
        })
        .unwrap();
        store.remember_at(turn("old", None), at(1_000)).unwrap();
        store.remember_at(turn("new", None), at(1_030)).unwrap();
        assert!(store.contains("old").unwrap(), "still inside the ttl");

        store.remember_at(turn("newer", None), at(1_085)).unwrap();
        assert!(!store.contains("old").unwrap());
        assert!(store.contains("new").unwrap());
    }

    #[test]
    fn the_byte_cap_evicts_least_recently_touched_first() {
        let store = capped(3);
        for (n, ts) in [("r1", 1_000), ("r2", 1_001), ("r3", 1_002)] {
            store.remember_at(turn(n, None), at(ts)).unwrap();
        }
        store.chain_at("r1", at(1_003)).unwrap().unwrap();

        store.remember_at(turn("r4", None), at(1_004)).unwrap();
        assert_eq!(store.len().unwrap(), 3, "the cap held");
        assert!(!store.contains("r2").unwrap(), "least recently touched");
        assert!(store.contains("r1").unwrap(), "touched back to the front");
        assert!(store.contains("r4").unwrap());
    }

    #[test]
    fn an_evicted_ancestor_breaks_the_chain_rather_than_truncating_it() {
        let store = store();
        store.remember(turn("r1", None)).unwrap();
        store.remember(turn("r2", Some("r1"))).unwrap();
        store.remember(turn("r3", Some("r2"))).unwrap();

        {
            let inner = store.lock();
            inner
                .conn
                .execute("DELETE FROM response WHERE id = 'r2'", [])
                .unwrap();
        }
        assert!(store.chain("r3").unwrap().is_none(), "hole in the middle");
        assert!(store.chain("r1").unwrap().is_some(), "the root still works");
    }

    #[test]
    fn a_turn_larger_than_the_whole_store_is_skipped() {
        let store = capped(2);
        store.remember(turn("small", None)).unwrap();
        let before = store.bytes().unwrap();

        let mut huge = turn("huge", None);
        huge.input = vec![json!("x".repeat(400))];
        store.remember(huge).unwrap();

        assert!(!store.contains("huge").unwrap());
        assert!(store.contains("small").unwrap());
        assert_eq!(store.bytes().unwrap(), before);
    }

    #[test]
    fn remembering_an_id_twice_replaces_it_without_leaking_bytes() {
        let store = store();
        store.remember(turn("r1", None)).unwrap();
        let once = store.bytes().unwrap();

        let mut again = turn("r1", None);
        again.output = vec![json!({ "type": "message", "content": "rewritten       " })];
        store.remember(again).unwrap();

        assert_eq!(store.len().unwrap(), 1);
        assert_eq!(
            store.bytes().unwrap(),
            once,
            "the old row's bytes are given back"
        );
        let chain = store.chain("r1").unwrap().unwrap();
        assert_eq!(contents(&chain), vec!["in r1", "rewritten"]);
    }

    #[test]
    fn a_parent_cycle_does_not_hang() {
        let store = store();
        store.remember(turn("r1", Some("r2"))).unwrap();
        store.remember(turn("r2", Some("r1"))).unwrap();
        assert!(store.chain("r1").unwrap().is_none());
    }

    #[test]
    fn chains_and_placements_survive_a_restart() {
        let dir = temp_dir("transcripts-reopen");
        let path = dir.join("transcripts.db");

        let store = TranscriptStore::open(&path, Limits::default()).unwrap();
        store.remember(turn("r1", None)).unwrap();
        store.remember(turn("r2", Some("r1"))).unwrap();
        store.place("k1", &key()).unwrap();
        let bytes = store.bytes().unwrap();
        drop(store);

        let store = TranscriptStore::open(&path, Limits::default()).unwrap();
        let chain = store.chain("r2").unwrap().expect("chain survived");
        assert_eq!(contents(&chain), vec!["in r1", "out r1", "in r2", "out r2"]);
        assert_eq!(store.placement("k1").unwrap(), Some(key()));
        assert_eq!(
            store.bytes().unwrap(),
            bytes,
            "the running total is reloaded, not restarted at zero"
        );
    }

    #[test]
    fn a_placement_is_read_back_and_touched_by_the_reading() {
        let store = store();
        assert!(store.placement("k1").unwrap().is_none(), "never placed");

        store.place_at("k1", &key(), at(1_000)).unwrap();
        assert_eq!(store.placement_at("k1", at(1_500)).unwrap(), Some(key()));
        assert!(
            store.placement("k2").unwrap().is_none(),
            "keys are separate"
        );

        let store = TranscriptStore::in_memory(Limits {
            max_bytes: u64::MAX,
            ttl: Duration::from_secs(600),
        })
        .unwrap();
        store.place_at("k1", &key(), at(1_000)).unwrap();
        assert!(store.placement_at("k1", at(1_500)).unwrap().is_some());
        assert!(
            store.placement_at("k1", at(2_000)).unwrap().is_some(),
            "1000 seconds after it was placed, but 500 after it was last used"
        );
    }

    #[test]
    fn a_placement_that_moves_replaces_the_old_one() {
        let store = store();
        let moved = SubKey::new(Provider::Codex, "acct-2");
        store.place("k1", &key()).unwrap();
        store.place("k1", &moved).unwrap();

        assert_eq!(store.placement("k1").unwrap(), Some(moved));
        let inner = store.lock();
        let rows: i64 = inner
            .conn
            .query_row("SELECT COUNT(*) FROM placement", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1, "a key is placed in one account, not appended to");
    }

    #[test]
    fn a_placement_unused_for_the_ttl_expires_and_is_swept() {
        let store = TranscriptStore::in_memory(Limits {
            max_bytes: u64::MAX,
            ttl: Duration::from_secs(60),
        })
        .unwrap();
        store.place_at("k1", &key(), at(1_000)).unwrap();
        assert!(store.placement_at("k1", at(1_030)).unwrap().is_some());

        assert!(
            store.placement_at("k1", at(1_200)).unwrap().is_none(),
            "an expired placement is not followed even before it is swept"
        );
        // Eviction rides on a written turn, as it does for a dead chain.
        store.remember_at(turn("r1", None), at(1_200)).unwrap();
        let inner = store.lock();
        let rows: i64 = inner
            .conn
            .query_row("SELECT COUNT(*) FROM placement", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[cfg(unix)]
    #[test]
    fn the_database_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = temp_dir("transcripts-mode");
        let path = dir.join("nested").join("transcripts.db");
        let store = TranscriptStore::open(&path, Limits::default()).unwrap();
        store.remember(turn("r1", None)).unwrap();

        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            super::super::FILE_MODE
        );
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            super::super::DIR_MODE
        );
    }
}
