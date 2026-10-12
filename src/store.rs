use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS speakers (
    id          INTEGER PRIMARY KEY,
    key         TEXT NOT NULL UNIQUE,   -- uid:<n> | cert:<hash> | name:<name>
    name        TEXT NOT NULL,
    user_id     INTEGER,
    cert_hash   TEXT,
    first_seen  INTEGER NOT NULL,
    last_seen   INTEGER NOT NULL,
    rev         INTEGER NOT NULL DEFAULT 0  -- change number for publishing (see Store::rev)
);

CREATE TABLE IF NOT EXISTS channels (
    id          INTEGER PRIMARY KEY,
    mumble_id   INTEGER NOT NULL,
    parent_id   INTEGER,                -- internal id of parent
    name        TEXT NOT NULL,
    position    INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL,
    removed_at  INTEGER,
    rev         INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS channels_mumble ON channels(mumble_id);

CREATE TABLE IF NOT EXISTS segments (
    id          INTEGER PRIMARY KEY,
    speaker_id  INTEGER NOT NULL REFERENCES speakers(id),
    channel_id  INTEGER NOT NULL REFERENCES channels(id),
    session     INTEGER NOT NULL,
    target      INTEGER NOT NULL DEFAULT 0, -- 0 = normal talk, 1..30 = whisper/shout, 31 = loopback
    start_ms    INTEGER NOT NULL,
    end_ms      INTEGER NOT NULL,
    samples     INTEGER NOT NULL DEFAULT 0,
    status      TEXT NOT NULL,              -- recording | raw | ready | error
    path        TEXT,                       -- relative to data dir (a day pack or a raw file)
    pack_offset INTEGER,                    -- byte offset inside the pack; NULL = whole file
    size        INTEGER NOT NULL DEFAULT 0, -- stored bytes (length inside the pack)
    raw_size    INTEGER NOT NULL DEFAULT 0,
    peaks       BLOB,                       -- PEAKS_PER_SEC values/s, sqrt-scaled u8 peak
    error       TEXT,
    published   INTEGER NOT NULL DEFAULT 0  -- 1 = appended to its day log
);
CREATE INDEX IF NOT EXISTS segments_start ON segments(start_ms);
CREATE INDEX IF NOT EXISTS segments_status ON segments(status);

CREATE TABLE IF NOT EXISTS presence (
    id          INTEGER PRIMARY KEY,
    speaker_id  INTEGER NOT NULL REFERENCES speakers(id),
    channel_id  INTEGER NOT NULL REFERENCES channels(id),
    session     INTEGER NOT NULL,
    start_ms    INTEGER NOT NULL,
    end_ms      INTEGER,
    pub         INTEGER NOT NULL DEFAULT 0  -- bit 0: open published, bit 1: close published
);
CREATE INDEX IF NOT EXISTS presence_start ON presence(start_ms);

CREATE TABLE IF NOT EXISTS uptime (
    id          INTEGER PRIMARY KEY,
    start_ms    INTEGER NOT NULL,
    end_ms      INTEGER NOT NULL,           -- last heartbeat while running
    server      TEXT NOT NULL,
    closed      INTEGER NOT NULL DEFAULT 0, -- 1 = session over, end_ms is final
    pub         INTEGER NOT NULL DEFAULT 0  -- bit 0: open published, bit 1: close published
);
CREATE INDEX IF NOT EXISTS uptime_start ON uptime(start_ms);

CREATE TABLE IF NOT EXISTS kv (
    k           TEXT PRIMARY KEY,
    v           INTEGER NOT NULL
);
"#;

/// Columns added after the first release: (table, column, declaration).
const ADDED_COLUMNS: &[(&str, &str, &str)] = &[
    ("speakers", "rev", "INTEGER NOT NULL DEFAULT 0"),
    ("channels", "rev", "INTEGER NOT NULL DEFAULT 0"),
    ("segments", "published", "INTEGER NOT NULL DEFAULT 0"),
    ("presence", "pub", "INTEGER NOT NULL DEFAULT 0"),
    ("uptime", "closed", "INTEGER NOT NULL DEFAULT 0"),
    ("uptime", "pub", "INTEGER NOT NULL DEFAULT 0"),
];

/// Indexes on added columns (created after the migration).
const INDEXES: &str = r#"
CREATE INDEX IF NOT EXISTS segments_unpublished ON segments(start_ms) WHERE published = 0;
CREATE INDEX IF NOT EXISTS presence_unpublished ON presence(id) WHERE pub < 3;
CREATE INDEX IF NOT EXISTS presence_end ON presence(end_ms);
CREATE INDEX IF NOT EXISTS uptime_unpublished ON uptime(id) WHERE pub < 3;
CREATE INDEX IF NOT EXISTS uptime_end ON uptime(end_ms);
CREATE INDEX IF NOT EXISTS channels_rev ON channels(rev);
CREATE INDEX IF NOT EXISTS speakers_rev ON speakers(rev);
"#;

pub struct Store {
    w: Mutex<Connection>,
    r: Mutex<Connection>,
    /// Last change number given to a channel/speaker row; the publisher emits
    /// rows whose `rev` is above the last one it published.
    rev: AtomicI64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Segment {
    pub id: i64,
    pub speaker_id: i64,
    pub channel_id: i64,
    pub session: i64,
    pub target: i64,
    pub start_ms: i64,
    pub end_ms: i64,
    pub status: String,
    /// Relative to the data dir (a day pack under audio/, or a file under raw/); served statically.
    pub path: Option<String>,
    /// Byte offset of the pack record; None means `path` is a standalone Ogg Opus file.
    pub offset: Option<i64>,
    pub size: i64,
}

/// One stored speech region of a burst.
pub struct Part {
    pub start_ms: i64,
    pub end_ms: i64,
    pub samples: i64,
    pub path: String,
    pub offset: i64,
    pub size: i64,
    pub peaks: Vec<u8>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Channel {
    pub id: i64,
    pub mumble_id: i64,
    pub parent_id: Option<i64>,
    pub name: String,
    pub position: i64,
    pub removed_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Track {
    pub channel_id: i64,
    pub speaker_id: i64,
    pub segments: i64,
    pub total_ms: i64,
    pub first_ms: i64,
    pub last_ms: i64,
}

/// A presence interval or bot session (`speaker_id`/`channel_id` are 0 for sessions).
#[derive(Debug, Clone)]
pub struct Interval {
    pub id: i64,
    pub speaker_id: i64,
    pub channel_id: i64,
    pub start_ms: i64,
    /// None while open.
    pub end_ms: Option<i64>,
    /// Publishing state: bit 0 open published, bit 1 close published.
    pub pub_state: i64,
}

/// A finished segment as published in a day log.
#[derive(Debug, Clone)]
pub struct PubSegment {
    pub id: i64,
    pub speaker_id: i64,
    pub channel_id: i64,
    pub target: i64,
    pub start_ms: i64,
    pub end_ms: i64,
    pub error: bool,
    pub path: String,
    pub offset: Option<i64>,
    pub size: i64,
    pub raw_size: i64,
    pub peaks: Vec<u8>,
}

/// Totals of the segments starting in a time range.
#[derive(Debug, Clone, Default)]
pub struct Totals {
    pub segments: i64,
    pub total_ms: i64,
    pub bytes: i64,
    pub raw_bytes: i64,
    pub first_ms: i64,
    pub last_ms: i64,
    /// Segments still being processed.
    pub pending: i64,
}

/// Which recordings a query should cover. Empty lists mean "no restriction".
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub channels: Vec<i64>,
    pub speakers: Vec<i64>,
    /// (channel, speaker) pairs; when set, overrides the two lists above.
    pub tracks: Vec<(i64, i64)>,
}

impl Filter {
    fn sql(&self) -> String {
        let list = |v: &[i64]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(",");
        let mut s = String::new();
        if !self.tracks.is_empty() {
            let pairs = self.tracks.iter().map(|(c, sp)| format!("(channel_id={c} AND speaker_id={sp})")).collect::<Vec<_>>();
            s += &format!(" AND ({})", pairs.join(" OR "));
            return s;
        }
        if !self.channels.is_empty() {
            s += &format!(" AND channel_id IN ({})", list(&self.channels));
        }
        if !self.speakers.is_empty() {
            s += &format!(" AND speaker_id IN ({})", list(&self.speakers));
        }
        s
    }
}

/// Upper bound on segment duration used to make overlap queries index-friendly.
pub const MAX_SEGMENT_MS: i64 = 3_600_000;

fn seg_from_row(r: &rusqlite::Row) -> rusqlite::Result<Segment> {
    Ok(Segment {
        id: r.get(0)?,
        speaker_id: r.get(1)?,
        channel_id: r.get(2)?,
        session: r.get(3)?,
        target: r.get(4)?,
        start_ms: r.get(5)?,
        end_ms: r.get(6)?,
        status: r.get(7)?,
        path: r.get(8)?,
        size: r.get(9)?,
        offset: r.get(10)?,
    })
}

const SEG_COLS: &str = "id, speaker_id, channel_id, session, target, start_ms, end_ms, status, path, size, pack_offset";

const PUB_SEG_COLS: &str =
    "id, speaker_id, channel_id, target, start_ms, end_ms, status, COALESCE(path, ''), pack_offset, size, raw_size, COALESCE(peaks, x'')";

fn pub_seg_row(r: &rusqlite::Row) -> rusqlite::Result<PubSegment> {
    Ok(PubSegment {
        id: r.get(0)?,
        speaker_id: r.get(1)?,
        channel_id: r.get(2)?,
        target: r.get(3)?,
        start_ms: r.get(4)?,
        end_ms: r.get(5)?,
        error: r.get::<_, String>(6)? != "ready",
        path: r.get(7)?,
        offset: r.get(8)?,
        size: r.get(9)?,
        raw_size: r.get(10)?,
        peaks: r.get(11)?,
    })
}

fn interval_row(r: &rusqlite::Row) -> rusqlite::Result<Interval> {
    Ok(Interval {
        id: r.get(0)?,
        speaker_id: r.get(1)?,
        channel_id: r.get(2)?,
        start_ms: r.get(3)?,
        end_ms: r.get(4)?,
        pub_state: r.get(5)?,
    })
}

const CHANNEL_COLS: &str = "id, mumble_id, parent_id, name, position, removed_at";

fn channel_row(r: &rusqlite::Row) -> rusqlite::Result<Channel> {
    Ok(Channel {
        id: r.get(0)?,
        mumble_id: r.get(1)?,
        parent_id: r.get(2)?,
        name: r.get(3)?,
        position: r.get(4)?,
        removed_at: r.get(5)?,
    })
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let w = Connection::open(path)?;
        w.busy_timeout(std::time::Duration::from_secs(10))?;
        w.execute_batch(SCHEMA)?;
        for (table, col, decl) in ADDED_COLUMNS {
            let has = w.prepare(&format!("SELECT 1 FROM pragma_table_info('{table}') WHERE name = '{col}'"))?.exists([])?;
            if !has {
                w.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {col} {decl}"))?;
            }
        }
        w.execute_batch(INDEXES)?;
        let rev: i64 = w.query_row(
            "SELECT MAX(COALESCE((SELECT MAX(rev) FROM channels), 0), COALESCE((SELECT MAX(rev) FROM speakers), 0))",
            [],
            |r| r.get(0),
        )?;
        let r = Connection::open(path)?;
        r.busy_timeout(std::time::Duration::from_secs(10))?;
        Ok(Self { w: Mutex::new(w), r: Mutex::new(r), rev: AtomicI64::new(rev) })
    }

    fn next_rev(&self) -> i64 {
        self.rev.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn w(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.w.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn r(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.r.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---- speakers ----

    pub fn upsert_speaker(&self, key: &str, name: &str, user_id: Option<i64>, hash: Option<&str>, now: i64) -> Result<i64> {
        let c = self.w();
        // rev only moves when something the UI shows (name, user id) changes.
        c.execute(
            "INSERT INTO speakers(key, name, user_id, cert_hash, first_seen, last_seen, rev) VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)
             ON CONFLICT(key) DO UPDATE SET name = excluded.name, last_seen = excluded.last_seen,
                user_id = COALESCE(excluded.user_id, user_id), cert_hash = COALESCE(excluded.cert_hash, cert_hash),
                rev = CASE WHEN name IS NOT excluded.name OR user_id IS NOT COALESCE(excluded.user_id, user_id)
                           THEN excluded.rev ELSE rev END",
            params![key, name, user_id, hash, now, self.next_rev()],
        )?;
        Ok(c.query_row("SELECT id FROM speakers WHERE key = ?1", [key], |r| r.get(0))?)
    }

    pub fn touch_speaker(&self, id: i64, now: i64) -> Result<()> {
        self.w().execute("UPDATE speakers SET last_seen = ?2 WHERE id = ?1", params![id, now])?;
        Ok(())
    }

    /// Drop a speaker row that only exists because of the bot's own pre-sync UserState.
    pub fn forget_speaker_if_unused(&self, id: i64) -> Result<()> {
        let c = self.w();
        let used: i64 = c.query_row("SELECT COUNT(*) FROM segments WHERE speaker_id = ?1", [id], |r| r.get(0))?;
        if used == 0 {
            c.execute("DELETE FROM presence WHERE speaker_id = ?1", [id])?;
            c.execute("DELETE FROM speakers WHERE id = ?1", [id])?;
        }
        Ok(())
    }

    // ---- channels ----

    /// Map a live Mumble channel onto a stable internal channel row.
    pub fn resolve_channel(&self, mumble_id: u32, name: &str, parent: Option<i64>, position: i64, now: i64) -> Result<i64> {
        let c = self.w();
        // Same id + same name: the same channel (e.g. after a reconnect).
        let same: Option<i64> = c
            .query_row(
                "SELECT id FROM channels WHERE mumble_id = ?1 AND name = ?2 ORDER BY id DESC LIMIT 1",
                params![mumble_id, name],
                |r| r.get(0),
            )
            .optional()?;
        // Same id, still alive, different name: renamed while we were away.
        let id = match same {
            Some(id) => Some(id),
            None => c
                .query_row(
                    "SELECT id FROM channels WHERE mumble_id = ?1 AND removed_at IS NULL ORDER BY id DESC LIMIT 1",
                    [mumble_id],
                    |r| r.get(0),
                )
                .optional()?,
        };
        match id {
            Some(id) => {
                c.execute(
                    "UPDATE channels SET name = ?2, parent_id = ?3, position = ?4, removed_at = NULL, rev = ?5
                     WHERE id = ?1 AND (name IS NOT ?2 OR parent_id IS NOT ?3 OR position IS NOT ?4 OR removed_at IS NOT NULL)",
                    params![id, name, parent, position, self.next_rev()],
                )?;
                Ok(id)
            }
            None => {
                c.execute(
                    "INSERT INTO channels(mumble_id, parent_id, name, position, created_at, rev) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![mumble_id, parent, name, position, now, self.next_rev()],
                )?;
                Ok(c.last_insert_rowid())
            }
        }
    }

    pub fn update_channel(&self, id: i64, name: &str, parent: Option<i64>, position: i64) -> Result<()> {
        self.w().execute(
            "UPDATE channels SET name = ?2, parent_id = ?3, position = ?4, rev = ?5
             WHERE id = ?1 AND (name IS NOT ?2 OR parent_id IS NOT ?3 OR position IS NOT ?4)",
            params![id, name, parent, position, self.next_rev()],
        )?;
        Ok(())
    }

    pub fn remove_channel(&self, id: i64, now: i64) -> Result<()> {
        self.w().execute("UPDATE channels SET removed_at = ?2, rev = ?3 WHERE id = ?1", params![id, now, self.next_rev()])?;
        Ok(())
    }

    /// Channels that existed in the DB but were not seen after a fresh sync.
    pub fn mark_channels_removed_except(&self, alive: &[i64], now: i64) -> Result<()> {
        let list = alive.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(",");
        self.w().execute(
            &format!("UPDATE channels SET removed_at = ?1, rev = ?2 WHERE removed_at IS NULL AND id NOT IN ({list})"),
            params![now, self.next_rev()],
        )?;
        Ok(())
    }

    /// Channels that currently exist on the server.
    pub fn live_channels(&self) -> Result<Vec<Channel>> {
        let c = self.r();
        let mut st = c.prepare(&format!("SELECT {CHANNEL_COLS} FROM channels WHERE removed_at IS NULL ORDER BY id"))?;
        let rows = st.query_map([], channel_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn channel_path(&self, id: i64) -> Result<String> {
        let c = self.r();
        let mut parts = vec![];
        let mut cur = Some(id);
        while let Some(id) = cur {
            if parts.len() > 64 {
                break;
            }
            let row: Option<(String, Option<i64>)> =
                c.query_row("SELECT name, parent_id FROM channels WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            match row {
                Some((n, p)) => {
                    parts.push(n);
                    cur = p;
                }
                None => break,
            }
        }
        parts.reverse();
        Ok(parts.join("/"))
    }

    pub fn speaker_name(&self, id: i64) -> Result<String> {
        Ok(self.r().query_row("SELECT name FROM speakers WHERE id = ?1", [id], |r| r.get(0)).optional()?.unwrap_or_default())
    }

    // ---- presence / uptime ----

    pub fn open_presence(&self, speaker: i64, channel: i64, session: u32, start: i64) -> Result<i64> {
        let c = self.w();
        c.execute(
            "INSERT INTO presence(speaker_id, channel_id, session, start_ms) VALUES (?1, ?2, ?3, ?4)",
            params![speaker, channel, session, start],
        )?;
        Ok(c.last_insert_rowid())
    }

    pub fn close_presence(&self, id: i64, end: i64) -> Result<()> {
        self.w().execute("UPDATE presence SET end_ms = MAX(start_ms, ?2) WHERE id = ?1", params![id, end])?;
        Ok(())
    }

    /// Close presence intervals left open by a crash, using the last uptime heartbeat.
    pub fn close_dangling(&self) -> Result<()> {
        let c = self.w();
        let last: Option<i64> = c.query_row("SELECT MAX(end_ms) FROM uptime", [], |r| r.get(0))?;
        let last = last.unwrap_or(0);
        c.execute("UPDATE presence SET end_ms = MAX(start_ms, ?1) WHERE end_ms IS NULL", [last])?;
        c.execute("UPDATE uptime SET closed = 1 WHERE closed = 0", [])?;
        Ok(())
    }

    pub fn start_uptime(&self, now: i64, server: &str) -> Result<i64> {
        let c = self.w();
        c.execute("INSERT INTO uptime(start_ms, end_ms, server) VALUES (?1, ?1, ?2)", params![now, server])?;
        Ok(c.last_insert_rowid())
    }

    pub fn touch_uptime(&self, id: i64, now: i64) -> Result<()> {
        self.w().execute("UPDATE uptime SET end_ms = ?2 WHERE id = ?1", params![id, now])?;
        Ok(())
    }

    /// End of a bot session (disconnect).
    pub fn close_uptime(&self, id: i64, now: i64) -> Result<()> {
        self.w().execute("UPDATE uptime SET end_ms = MAX(start_ms, ?2), closed = 1 WHERE id = ?1", params![id, now])?;
        Ok(())
    }

    /// Presence intervals overlapping [from, to). Split into index-friendly parts:
    /// a plain `start < to AND end > from` would scan all history before `to`.
    pub fn presence_overlapping(&self, from: i64, to: i64) -> Result<Vec<Interval>> {
        self.intervals("presence", "speaker_id, channel_id", "end_ms", from, to)
    }

    /// Bot sessions overlapping [from, to); open sessions have `end_ms` None.
    pub fn uptime_overlapping(&self, from: i64, to: i64) -> Result<Vec<Interval>> {
        self.intervals("uptime", "0, 0", "CASE WHEN closed = 1 THEN end_ms END", from, to)
    }

    /// Presence intervals / bot sessions starting in [from, to) (`table` is "presence" or "uptime").
    pub fn intervals_starting(&self, table: &str, from: i64, to: i64) -> Result<Vec<Interval>> {
        let (cols, end) = Self::interval_cols(table);
        let c = self.r();
        let mut st = c.prepare(&format!(
            "SELECT id, {cols}, start_ms, {end}, pub FROM {table} WHERE start_ms >= ?1 AND start_ms < ?2 ORDER BY start_ms"
        ))?;
        let rows = st.query_map(params![from, to], interval_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn interval_cols(table: &str) -> (&'static str, &'static str) {
        match table {
            "presence" => ("speaker_id, channel_id", "end_ms"),
            _ => ("0, 0", "CASE WHEN closed = 1 THEN end_ms END"),
        }
    }

    fn intervals(&self, table: &str, cols: &str, end: &str, from: i64, to: i64) -> Result<Vec<Interval>> {
        let c = self.r();
        let open = if table == "uptime" { "closed = 0" } else { "end_ms IS NULL" };
        let sql = format!(
            "SELECT id, {cols}, start_ms, {end}, pub FROM {table} WHERE start_ms >= ?1 AND start_ms < ?2
             UNION ALL SELECT id, {cols}, start_ms, {end}, pub FROM {table} WHERE end_ms > ?1 AND start_ms < ?1 AND NOT ({open})
             UNION ALL SELECT id, {cols}, start_ms, {end}, pub FROM {table} WHERE {open} AND start_ms < ?1
             ORDER BY 4"
        );
        let mut st = c.prepare(&sql)?;
        let rows = st.query_map(params![from, to], interval_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Presence/uptime rows with unpublished changes (open or close).
    pub fn unpublished_intervals(&self, table: &str) -> Result<Vec<Interval>> {
        let c = self.r();
        let sql = match table {
            "presence" => "SELECT id, speaker_id, channel_id, start_ms, end_ms, pub FROM presence
                           WHERE pub < 3 AND NOT (pub = 1 AND end_ms IS NULL) ORDER BY id",
            _ => "SELECT id, 0, 0, start_ms, CASE WHEN closed = 1 THEN end_ms END, pub FROM uptime
                  WHERE pub < 3 AND NOT (pub = 1 AND closed = 0) ORDER BY id",
        };
        let mut st = c.prepare(sql)?;
        let rows = st.query_map([], interval_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_interval_pub(&self, table: &str, ids: &[(i64, i64)]) -> Result<()> {
        let mut c = self.w();
        let tx = c.transaction()?;
        {
            let mut st = tx.prepare(&format!("UPDATE {table} SET pub = ?2 WHERE id = ?1"))?;
            for (id, p) in ids {
                st.execute(params![id, p])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ---- segments ----

    /// A complete burst waiting to be transcoded.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_raw_segment(
        &self,
        speaker: i64,
        channel: i64,
        session: u32,
        target: u8,
        start: i64,
        end: i64,
        samples: i64,
        raw_path: &str,
        size: i64,
    ) -> Result<i64> {
        let c = self.w();
        c.execute(
            "INSERT INTO segments(speaker_id, channel_id, session, target, start_ms, end_ms, samples, status, path, size, raw_size)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'raw', ?8, ?9, ?9)",
            params![speaker, channel, session, target, start, end, samples, raw_path, size],
        )?;
        Ok(c.last_insert_rowid())
    }

    /// Legacy: completes a 'recording' row left by versions that created rows at burst start.
    pub fn finish_segment(&self, id: i64, end: i64, samples: i64, raw_size: i64) -> Result<()> {
        self.w().execute(
            "UPDATE segments SET end_ms = ?2, samples = ?3, raw_size = ?4, size = ?4, status = 'raw' WHERE id = ?1",
            params![id, end, samples, raw_size],
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn segment_done(
        &self,
        id: i64,
        path: &str,
        offset: Option<i64>,
        size: i64,
        peaks: &[u8],
        status: &str,
        error: Option<&str>,
    ) -> Result<()> {
        self.w().execute(
            "UPDATE segments SET path = ?2, pack_offset = ?3, size = ?4, peaks = ?5, status = ?6, error = ?7 WHERE id = ?1",
            params![id, path, offset, size, peaks, status, error],
        )?;
        Ok(())
    }

    /// Replace a raw burst with its stored speech regions. The first part keeps
    /// the burst's id; returns the ids of all parts.
    pub fn split_segment(&self, id: i64, parts: &[Part]) -> Result<Vec<i64>> {
        let mut c = self.w();
        let tx = c.transaction()?;
        let mut ids = vec![];
        for (i, p) in parts.iter().enumerate() {
            if i == 0 {
                tx.execute(
                    "UPDATE segments SET start_ms = ?2, end_ms = ?3, samples = ?4, path = ?5, pack_offset = ?6, size = ?7,
                        peaks = ?8, status = 'ready', error = NULL WHERE id = ?1",
                    params![id, p.start_ms, p.end_ms, p.samples, p.path, p.offset, p.size, p.peaks],
                )?;
                ids.push(id);
            } else {
                tx.execute(
                    "INSERT INTO segments(speaker_id, channel_id, session, target, start_ms, end_ms, samples, status, path,
                        pack_offset, size, peaks)
                     SELECT speaker_id, channel_id, session, target, ?2, ?3, ?4, 'ready', ?5, ?6, ?7, ?8 FROM segments WHERE id = ?1",
                    params![id, p.start_ms, p.end_ms, p.samples, p.path, p.offset, p.size, p.peaks],
                )?;
                ids.push(tx.last_insert_rowid());
            }
        }
        tx.commit()?;
        Ok(ids)
    }

    pub fn path_in_use(&self, path: &str) -> Result<bool> {
        let n: i64 = self.r().query_row("SELECT COUNT(*) FROM segments WHERE path = ?1", [path], |r| r.get(0))?;
        Ok(n > 0)
    }

    pub fn delete_segment(&self, id: i64) -> Result<()> {
        self.w().execute("DELETE FROM segments WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn segments_by_status(&self, status: &str) -> Result<Vec<Segment>> {
        let c = self.r();
        let mut st = c.prepare(&format!("SELECT {SEG_COLS} FROM segments WHERE status = ?1 ORDER BY id"))?;
        let rows = st.query_map([status], seg_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn segment(&self, id: i64) -> Result<Option<Segment>> {
        let c = self.r();
        Ok(c.query_row(&format!("SELECT {SEG_COLS} FROM segments WHERE id = ?1"), [id], seg_from_row).optional()?)
    }

    /// Segments overlapping [from, to), ordered by start.
    pub fn segments(&self, from: i64, to: i64, f: &Filter) -> Result<Vec<Segment>> {
        let c = self.r();
        let sql = format!(
            "SELECT {SEG_COLS} FROM segments WHERE start_ms >= ?1 AND start_ms < ?2 AND end_ms > ?3 {} ORDER BY start_ms",
            f.sql()
        );
        let mut st = c.prepare(&sql)?;
        let rows = st.query_map(params![from - MAX_SEGMENT_MS, to, from], seg_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Finished segments not yet appended to a day log, oldest first.
    pub fn unpublished_segments(&self, limit: i64) -> Result<Vec<PubSegment>> {
        let c = self.r();
        let mut st = c.prepare(&format!(
            "SELECT {PUB_SEG_COLS} FROM segments WHERE published = 0 AND status IN ('ready','error') ORDER BY start_ms LIMIT ?1"
        ))?;
        let rows = st.query_map([limit], pub_seg_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Finished segments starting in [from, to) (full day log rebuild). Peaks are
    /// empty for segments already published: the day logs hold them.
    pub fn finished_segments(&self, from: i64, to: i64) -> Result<Vec<PubSegment>> {
        let c = self.r();
        let mut st = c.prepare(&format!(
            "SELECT {PUB_SEG_COLS} FROM segments WHERE start_ms >= ?1 AND start_ms < ?2 AND status IN ('ready','error') ORDER BY start_ms"
        ))?;
        let rows = st.query_map(params![from, to], pub_seg_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn mark_published(&self, ids: &[i64]) -> Result<()> {
        let mut c = self.w();
        let tx = c.transaction()?;
        {
            // The day log now holds the waveform peaks; the database doesn't need them.
            let mut st = tx.prepare("UPDATE segments SET published = 1, peaks = NULL WHERE id = ?1")?;
            for id in ids {
                st.execute([id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// First segment starting after `t` (dir > 0) or last segment ending before `t` (dir < 0).
    pub fn neighbour(&self, t: i64, dir: i32, f: &Filter) -> Result<Option<Segment>> {
        let c = self.r();
        let sql = if dir >= 0 {
            format!("SELECT {SEG_COLS} FROM segments WHERE start_ms > ?1 {} ORDER BY start_ms LIMIT 1", f.sql())
        } else {
            format!("SELECT {SEG_COLS} FROM segments WHERE start_ms < ?1 {} ORDER BY start_ms DESC LIMIT 1", f.sql())
        };
        Ok(c.query_row(&sql, [t], seg_from_row).optional()?)
    }

    pub fn tracks(&self, from: i64, to: i64) -> Result<Vec<Track>> {
        let c = self.r();
        let mut st = c.prepare(
            "SELECT channel_id, speaker_id, COUNT(*), SUM(end_ms - start_ms), MIN(start_ms), MAX(end_ms) FROM segments
             WHERE start_ms >= ?1 AND start_ms < ?2 AND end_ms > ?3 GROUP BY channel_id, speaker_id",
        )?;
        let rows = st.query_map(params![from - MAX_SEGMENT_MS, to, from], |r| {
            Ok(Track {
                channel_id: r.get(0)?,
                speaker_id: r.get(1)?,
                segments: r.get(2)?,
                total_ms: r.get(3)?,
                first_ms: r.get(4)?,
                last_ms: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Totals of the segments starting in [from, to) (indexed range scan).
    pub fn totals(&self, from: i64, to: i64) -> Result<Totals> {
        let c = self.r();
        Ok(c.query_row(
            "SELECT COUNT(*), COALESCE(SUM(end_ms - start_ms), 0), COALESCE(SUM(size), 0), COALESCE(SUM(raw_size), 0),
                    COALESCE(MIN(start_ms), 0), COALESCE(MAX(end_ms), 0),
                    COALESCE(SUM(status IN ('raw', 'recording')), 0)
             FROM segments WHERE start_ms >= ?1 AND start_ms < ?2",
            params![from, to],
            |r| {
                Ok(Totals {
                    segments: r.get(0)?,
                    total_ms: r.get(1)?,
                    bytes: r.get(2)?,
                    raw_bytes: r.get(3)?,
                    first_ms: r.get(4)?,
                    last_ms: r.get(5)?,
                    pending: r.get(6)?,
                })
            },
        )?)
    }

    /// Earliest time anything was recorded (segments, presence or sessions).
    pub fn earliest_ms(&self) -> Result<Option<i64>> {
        let c = self.r();
        Ok(c.query_row(
            "SELECT MIN(t) FROM (SELECT MIN(start_ms) AS t FROM segments UNION ALL SELECT MIN(start_ms) FROM presence
             UNION ALL SELECT MIN(start_ms) FROM uptime)",
            [],
            |r| r.get(0),
        )?)
    }

    /// Delete the segments starting in [from, to); returns the files they used.
    pub fn delete_segments_in(&self, from: i64, to: i64) -> Result<Vec<String>> {
        let c = self.w();
        let mut st = c.prepare("SELECT DISTINCT path FROM segments WHERE start_ms >= ?1 AND start_ms < ?2 AND path IS NOT NULL")?;
        let paths = st.query_map(params![from, to], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
        c.execute("DELETE FROM segments WHERE start_ms >= ?1 AND start_ms < ?2", params![from, to])?;
        Ok(paths)
    }

    pub fn prune_presence_before(&self, t: i64) -> Result<()> {
        let c = self.w();
        c.execute("DELETE FROM presence WHERE end_ms IS NOT NULL AND end_ms < ?1", [t])?;
        c.execute("DELETE FROM uptime WHERE closed = 1 AND end_ms < ?1", [t])?;
        Ok(())
    }

    pub fn channel(&self, id: i64) -> Result<Option<Channel>> {
        let c = self.r();
        Ok(c.query_row(&format!("SELECT {CHANNEL_COLS} FROM channels WHERE id = ?1"), [id], channel_row).optional()?)
    }

    /// Channel rows changed after revision `rev`, with their revision.
    pub fn channels_since(&self, rev: i64) -> Result<Vec<(Channel, i64)>> {
        let c = self.r();
        let mut st = c.prepare(&format!("SELECT {CHANNEL_COLS}, rev FROM channels WHERE rev > ?1 ORDER BY rev"))?;
        let rows = st.query_map([rev], |r| Ok((channel_row(r)?, r.get(6)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// (id, name, user_id) of a speaker.
    pub fn speaker(&self, id: i64) -> Result<Option<(i64, String, Option<i64>)>> {
        let c = self.r();
        Ok(c.query_row("SELECT id, name, user_id FROM speakers WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?)
    }

    /// Speaker ids whose name/user id changed after revision `rev`, and the highest revision seen.
    pub fn speakers_since(&self, rev: i64) -> Result<(Vec<i64>, i64)> {
        let c = self.r();
        let mut st = c.prepare("SELECT id, rev FROM speakers WHERE rev > ?1 ORDER BY rev")?;
        let rows = st.query_map([rev], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let max = rows.iter().map(|r| r.1).max().unwrap_or(rev);
        Ok((rows.into_iter().map(|r| r.0).collect(), max))
    }

    pub fn current_rev(&self) -> i64 {
        self.rev.load(Ordering::Relaxed)
    }

    /// Return space freed by deleted data to the filesystem (slow on big databases; rare use).
    pub fn vacuum(&self) -> Result<()> {
        self.w().execute_batch("VACUUM")?;
        Ok(())
    }

    pub fn kv_get(&self, k: &str) -> Result<Option<i64>> {
        Ok(self.r().query_row("SELECT v FROM kv WHERE k = ?1", [k], |r| r.get(0)).optional()?)
    }

    pub fn kv_set(&self, k: &str, v: i64) -> Result<()> {
        self.w().execute("INSERT INTO kv(k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = excluded.v", params![k, v])?;
        Ok(())
    }

    /// After a full rebuild of the logs: everything currently stored counts as published.
    pub fn mark_all_published(&self) -> Result<()> {
        let c = self.w();
        c.execute_batch(
            "UPDATE segments SET published = 1 WHERE published = 0 AND status IN ('ready','error');
             UPDATE segments SET peaks = NULL WHERE published = 1 AND peaks IS NOT NULL;
             UPDATE presence SET pub = CASE WHEN end_ms IS NULL THEN 1 ELSE 3 END WHERE pub < 3;
             UPDATE uptime SET pub = CASE WHEN closed = 0 THEN 1 ELSE 3 END WHERE pub < 3;",
        )?;
        Ok(())
    }
}
