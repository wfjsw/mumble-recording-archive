use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::path::Path;
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
    last_seen   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS channels (
    id          INTEGER PRIMARY KEY,
    mumble_id   INTEGER NOT NULL,
    parent_id   INTEGER,                -- internal id of parent
    name        TEXT NOT NULL,
    position    INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL,
    removed_at  INTEGER
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
    error       TEXT
);
CREATE INDEX IF NOT EXISTS segments_start ON segments(start_ms);
CREATE INDEX IF NOT EXISTS segments_status ON segments(status);

CREATE TABLE IF NOT EXISTS presence (
    id          INTEGER PRIMARY KEY,
    speaker_id  INTEGER NOT NULL REFERENCES speakers(id),
    channel_id  INTEGER NOT NULL REFERENCES channels(id),
    session     INTEGER NOT NULL,
    start_ms    INTEGER NOT NULL,
    end_ms      INTEGER
);
CREATE INDEX IF NOT EXISTS presence_start ON presence(start_ms);

CREATE TABLE IF NOT EXISTS uptime (
    id          INTEGER PRIMARY KEY,
    start_ms    INTEGER NOT NULL,
    end_ms      INTEGER NOT NULL,
    server      TEXT NOT NULL
);
"#;

pub struct Store {
    w: Mutex<Connection>,
    r: Mutex<Connection>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peaks: Option<String>, // base64
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
pub struct Speaker {
    pub id: i64,
    pub key: String,
    pub name: String,
    pub user_id: Option<i64>,
    pub first_seen: i64,
    pub last_seen: i64,
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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Presence {
    pub speaker_id: i64,
    pub channel_id: i64,
    pub start_ms: i64,
    pub end_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Interval {
    pub start_ms: i64,
    pub end_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Day {
    pub day: String,
    pub segments: i64,
    pub total_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    pub segments: i64,
    pub total_ms: i64,
    pub bytes: i64,
    pub raw_bytes: i64,
    pub earliest_ms: Option<i64>,
    pub latest_ms: Option<i64>,
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

fn seg_from_row(r: &rusqlite::Row, with_peaks: bool) -> rusqlite::Result<Segment> {
    use base64::Engine;
    let peaks: Option<Vec<u8>> = if with_peaks { r.get(11)? } else { None };
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
        peaks: peaks.map(|p| base64::engine::general_purpose::STANDARD.encode(p)),
    })
}

const SEG_COLS: &str = "id, speaker_id, channel_id, session, target, start_ms, end_ms, status, path, size, pack_offset";

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let w = Connection::open(path)?;
        w.busy_timeout(std::time::Duration::from_secs(10))?;
        w.execute_batch(SCHEMA)?;
        let r = Connection::open(path)?;
        r.busy_timeout(std::time::Duration::from_secs(10))?;
        Ok(Self { w: Mutex::new(w), r: Mutex::new(r) })
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
        c.execute(
            "INSERT INTO speakers(key, name, user_id, cert_hash, first_seen, last_seen) VALUES (?1, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT(key) DO UPDATE SET name = excluded.name, last_seen = excluded.last_seen,
                user_id = COALESCE(excluded.user_id, user_id), cert_hash = COALESCE(excluded.cert_hash, cert_hash)",
            params![key, name, user_id, hash, now],
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

    pub fn speakers(&self) -> Result<Vec<Speaker>> {
        let c = self.r();
        let mut st = c.prepare("SELECT id, key, name, user_id, first_seen, last_seen FROM speakers ORDER BY name COLLATE NOCASE")?;
        let rows = st.query_map([], |r| {
            Ok(Speaker { id: r.get(0)?, key: r.get(1)?, name: r.get(2)?, user_id: r.get(3)?, first_seen: r.get(4)?, last_seen: r.get(5)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
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
                    "UPDATE channels SET name = ?2, parent_id = ?3, position = ?4, removed_at = NULL WHERE id = ?1",
                    params![id, name, parent, position],
                )?;
                Ok(id)
            }
            None => {
                c.execute(
                    "INSERT INTO channels(mumble_id, parent_id, name, position, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![mumble_id, parent, name, position, now],
                )?;
                Ok(c.last_insert_rowid())
            }
        }
    }

    pub fn update_channel(&self, id: i64, name: &str, parent: Option<i64>, position: i64) -> Result<()> {
        self.w().execute(
            "UPDATE channels SET name = ?2, parent_id = ?3, position = ?4 WHERE id = ?1",
            params![id, name, parent, position],
        )?;
        Ok(())
    }

    pub fn remove_channel(&self, id: i64, now: i64) -> Result<()> {
        self.w().execute("UPDATE channels SET removed_at = ?2 WHERE id = ?1", params![id, now])?;
        Ok(())
    }

    /// Channels that existed in the DB but were not seen after a fresh sync.
    pub fn mark_channels_removed_except(&self, alive: &[i64], now: i64) -> Result<()> {
        let list = alive.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(",");
        self.w().execute(
            &format!("UPDATE channels SET removed_at = ?1 WHERE removed_at IS NULL AND id NOT IN ({list})"),
            [now],
        )?;
        Ok(())
    }

    pub fn channels(&self) -> Result<Vec<Channel>> {
        let c = self.r();
        let mut st = c.prepare("SELECT id, mumble_id, parent_id, name, position, removed_at FROM channels ORDER BY position, name COLLATE NOCASE")?;
        let rows = st.query_map([], |r| {
            Ok(Channel {
                id: r.get(0)?,
                mumble_id: r.get(1)?,
                parent_id: r.get(2)?,
                name: r.get(3)?,
                position: r.get(4)?,
                removed_at: r.get(5)?,
            })
        })?;
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

    pub fn presence(&self, from: i64, to: i64) -> Result<Vec<Presence>> {
        let c = self.r();
        let mut st = c.prepare(
            "SELECT speaker_id, channel_id, start_ms, end_ms FROM presence
             WHERE start_ms < ?2 AND (end_ms IS NULL OR end_ms > ?1) ORDER BY start_ms",
        )?;
        let rows = st.query_map(params![from, to], |r| {
            Ok(Presence { speaker_id: r.get(0)?, channel_id: r.get(1)?, start_ms: r.get(2)?, end_ms: r.get(3)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn uptime(&self, from: i64, to: i64) -> Result<Vec<Interval>> {
        let c = self.r();
        let mut st = c.prepare("SELECT start_ms, end_ms FROM uptime WHERE start_ms < ?2 AND end_ms > ?1 ORDER BY start_ms")?;
        let rows = st.query_map(params![from, to], |r| Ok(Interval { start_ms: r.get(0)?, end_ms: r.get(1)? }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
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
        let rows = st.query_map([status], |r| seg_from_row(r, false))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn segment(&self, id: i64) -> Result<Option<Segment>> {
        let c = self.r();
        Ok(c.query_row(&format!("SELECT {SEG_COLS}, peaks FROM segments WHERE id = ?1"), [id], |r| seg_from_row(r, true))
            .optional()?)
    }

    /// Segments overlapping [from, to), ordered by start.
    pub fn segments(&self, from: i64, to: i64, f: &Filter, with_peaks: bool) -> Result<Vec<Segment>> {
        let c = self.r();
        let sql = format!(
            "SELECT {SEG_COLS}, {} FROM segments WHERE start_ms >= ?1 AND start_ms < ?2 AND end_ms > ?3 {} ORDER BY start_ms",
            if with_peaks { "peaks" } else { "NULL" },
            f.sql()
        );
        let mut st = c.prepare(&sql)?;
        let rows = st.query_map(params![from - MAX_SEGMENT_MS, to, from], |r| seg_from_row(r, with_peaks))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Segments starting in [from, to) with peaks (for per-day indexes).
    pub fn segments_starting(&self, from: i64, to: i64) -> Result<Vec<Segment>> {
        let c = self.r();
        let mut st = c.prepare(&format!("SELECT {SEG_COLS}, peaks FROM segments WHERE start_ms >= ?1 AND start_ms < ?2 ORDER BY start_ms"))?;
        let rows = st.query_map(params![from, to], |r| seg_from_row(r, true))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn segments_by_ids(&self, ids: &[i64]) -> Result<Vec<Segment>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let list = ids.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(",");
        let c = self.r();
        let mut st = c.prepare(&format!("SELECT {SEG_COLS}, peaks FROM segments WHERE id IN ({list})"))?;
        let rows = st.query_map([], |r| seg_from_row(r, true))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// First segment starting after `t` (dir > 0) or last segment ending before `t` (dir < 0).
    pub fn neighbour(&self, t: i64, dir: i32, f: &Filter) -> Result<Option<Segment>> {
        let c = self.r();
        let sql = if dir >= 0 {
            format!("SELECT {SEG_COLS}, NULL FROM segments WHERE start_ms > ?1 {} ORDER BY start_ms LIMIT 1", f.sql())
        } else {
            format!("SELECT {SEG_COLS}, NULL FROM segments WHERE start_ms < ?1 {} ORDER BY start_ms DESC LIMIT 1", f.sql())
        };
        Ok(c.query_row(&sql, [t], |r| seg_from_row(r, false)).optional()?)
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

    /// Per-day totals; `tz_offset_min` is minutes east of UTC for the viewer.
    pub fn days(&self, tz_offset_min: i64) -> Result<Vec<Day>> {
        let c = self.r();
        let mut st = c.prepare(
            "SELECT date((start_ms + ?1 * 60000) / 1000, 'unixepoch') AS d, COUNT(*), SUM(end_ms - start_ms)
             FROM segments GROUP BY d ORDER BY d",
        )?;
        let rows = st.query_map([tz_offset_min], |r| Ok(Day { day: r.get(0)?, segments: r.get(1)?, total_ms: r.get(2)? }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn stats(&self) -> Result<Stats> {
        let c = self.r();
        Ok(c.query_row(
            "SELECT COUNT(*), COALESCE(SUM(end_ms - start_ms),0), COALESCE(SUM(size),0), COALESCE(SUM(raw_size),0),
                    MIN(start_ms), MAX(end_ms), (SELECT COUNT(*) FROM segments WHERE status IN ('raw','recording'))
             FROM segments",
            [],
            |r| {
                Ok(Stats {
                    segments: r.get(0)?,
                    total_ms: r.get(1)?,
                    bytes: r.get(2)?,
                    raw_bytes: r.get(3)?,
                    earliest_ms: r.get(4)?,
                    latest_ms: r.get(5)?,
                    pending: r.get(6)?,
                })
            },
        )?)
    }

    pub fn segments_before(&self, t: i64, limit: i64) -> Result<Vec<Segment>> {
        let c = self.r();
        let mut st = c.prepare(&format!(
            "SELECT {SEG_COLS}, NULL FROM segments WHERE end_ms < ?1 AND status IN ('ready','error') ORDER BY start_ms LIMIT ?2"
        ))?;
        let rows = st.query_map(params![t, limit], |r| seg_from_row(r, false))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn prune_presence_before(&self, t: i64) -> Result<()> {
        let c = self.w();
        c.execute("DELETE FROM presence WHERE end_ms IS NOT NULL AND end_ms < ?1", [t])?;
        c.execute("DELETE FROM uptime WHERE end_ms < ?1", [t])?;
        Ok(())
    }
}
