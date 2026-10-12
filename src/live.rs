//! Static publishing: everything the web UI needs is written as plain files
//! under the public dir, so the UI can be served by any static web server.
//!
//! Index files are append-only binary logs ([`crate::publog`]): the bot never
//! rewrites them, and the UI fetches only the bytes added since its last read
//! with an HTTP Range request. This keeps both the bot's writes and the
//! viewers' downloads proportional to what changed, however long the archive
//! runs.
//!
//! ```text
//! public/meta.json                      server info + current file names (written at startup / compaction)
//! public/days.log                       days that have data, and per-day totals once a day is finished
//! public/channels.<gen>.log             live channel tree; compacted into a new generation when it grows
//! public/index/YYYY-MM-DD.log           one day: speaker/channel names, segments, presence, bot sessions
//! public/audio/YYYY-MM/YYYY-MM-DD.pack  audio records (read with HTTP Range)
//! public/raw/…                          in-progress / fallback Ogg Opus files
//! ```
//!
//! A day log is self-contained: it defines every speaker and channel (with its
//! ancestors) it refers to, so no global speaker list is needed and removed
//! channels never accumulate in the live channel tree. The publisher derives
//! everything from the database (rows carry "published" flags or revision
//! numbers), so the voice loop never touches these files.

use crate::config::Config;
use crate::publog::{self, LogFile, Rec};
use crate::store::{Interval, PubSegment, Store};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Published format version (meta.json "format").
const FORMAT: u32 = 2;
/// A day's totals are published once it is over by this much and nothing of it is still processing.
const SUMMARY_GRACE_MS: i64 = 10 * 60_000;
/// Compact the channel log once it is this many times larger than its snapshot.
const COMPACT_FACTOR: u64 = 4;
const COMPACT_MIN_BYTES: u64 = 256 * 1024;
/// Day logs other than today's are closed after this long without writes.
const WRITER_IDLE_MS: i64 = 10 * 60_000;

pub fn day_key(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).unwrap_or_default().with_timezone(&chrono::Local).format("%Y-%m-%d").to_string()
}

/// [start, end) of a local calendar day in unix ms.
pub fn day_bounds(day: &str) -> Option<(i64, i64)> {
    use chrono::TimeZone;
    let d = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").ok()?;
    let at = |d: chrono::NaiveDate| {
        chrono::Local.from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap()).earliest().map(|t| t.timestamp_millis())
    };
    Some((at(d)?, at(d.succ_opt()?)?))
}

/// Day pack holding the audio of segments starting on `day` (see transcode).
fn pack_path(day: &str) -> String {
    format!("audio/{}/{day}.pack", &day[..7])
}

fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

// ---------- records ----------

fn channel_rec(c: &crate::store::Channel) -> Rec {
    Rec::new(publog::CHANNEL).u(c.id as u64).u(c.mumble_id as u64).opt(c.parent_id).i(c.position).str(&c.name)
}

fn segment_rec(s: &PubSegment, day_from: i64, pack: &str) -> Rec {
    Rec::new(publog::SEGMENT)
        .u(s.id as u64)
        .u(s.speaker_id as u64)
        .u(s.channel_id as u64)
        .i(s.start_ms - day_from)
        .u((s.end_ms - s.start_ms).max(0) as u64)
        .u(s.target as u64)
        .u(s.error as u64)
        .str(if s.path == pack { "" } else { &s.path })
        .opt(s.offset)
        .u(s.size as u64)
        .u(s.raw_size as u64)
        .bytes(&s.peaks)
}

// ---------- one day's log ----------

struct DayWriter {
    from: i64,
    pack: String,
    log: LogFile,
    speakers: HashSet<i64>,
    channels: HashSet<i64>,
    presence: HashSet<i64>,
    uptime: HashSet<i64>,
    last_write: i64,
}

impl DayWriter {
    fn path(public: &Path, day: &str) -> PathBuf {
        public.join("index").join(format!("{day}.log"))
    }

    fn create(public: &Path, day: &str, from: i64, to: i64) -> Result<Self> {
        let pack = pack_path(day);
        let header = Rec::new(publog::HEADER).u(publog::KIND_DAY as u64).str(day).u(from as u64).u(to as u64).str(&pack);
        let log = LogFile::create(&Self::path(public, day), header)?;
        Ok(Self::new(from, pack, log))
    }

    /// Reopen an existing day log, recovering which ids it already defines.
    fn open(public: &Path, day: &str, from: i64) -> Result<Self> {
        let (log, data) = LogFile::open(&Self::path(public, day))?;
        let mut w = Self::new(from, pack_path(day), log);
        let (recs, _) = publog::parse(&data)?;
        for (ty, mut f) in recs {
            let set = match ty {
                publog::SPEAKER => &mut w.speakers,
                publog::CHANNEL => &mut w.channels,
                publog::PRESENCE_OPEN => &mut w.presence,
                publog::UPTIME_OPEN => &mut w.uptime,
                _ => continue,
            };
            set.insert(f.u()? as i64);
        }
        Ok(w)
    }

    fn new(from: i64, pack: String, log: LogFile) -> Self {
        Self {
            from,
            pack,
            log,
            speakers: HashSet::new(),
            channels: HashSet::new(),
            presence: HashSet::new(),
            uptime: HashSet::new(),
            last_write: crate::bot::now_ms(),
        }
    }

    fn speaker(&mut self, store: &Store, id: i64, out: &mut Vec<Rec>) -> Result<()> {
        if self.speakers.insert(id) {
            if let Some((id, name, user_id)) = store.speaker(id)? {
                out.push(Rec::new(publog::SPEAKER).u(id as u64).opt(user_id).str(&name));
            }
        }
        Ok(())
    }

    /// Define a channel and any of its ancestors not yet defined in this day.
    fn channel(&mut self, store: &Store, id: i64, out: &mut Vec<Rec>) -> Result<()> {
        let mut chain = vec![];
        let mut cur = Some(id);
        while let Some(id) = cur {
            if chain.len() > 128 || !self.channels.insert(id) {
                break;
            }
            match store.channel(id)? {
                Some(c) => {
                    cur = c.parent_id;
                    chain.push(c);
                }
                None => break,
            }
        }
        out.extend(chain.iter().rev().map(channel_rec));
        Ok(())
    }

    fn presence_open(&mut self, store: &Store, p: &Interval, out: &mut Vec<Rec>) -> Result<()> {
        if self.presence.insert(p.id) {
            self.speaker(store, p.speaker_id, out)?;
            self.channel(store, p.channel_id, out)?;
            out.push(Rec::new(publog::PRESENCE_OPEN).u(p.id as u64).u(p.speaker_id as u64).u(p.channel_id as u64).i(p.start_ms - self.from));
        }
        Ok(())
    }

    fn uptime_open(&mut self, u: &Interval, out: &mut Vec<Rec>) {
        if self.uptime.insert(u.id) {
            out.push(Rec::new(publog::UPTIME_OPEN).u(u.id as u64).i(u.start_ms - self.from));
        }
    }

    fn segment(&mut self, store: &Store, s: &PubSegment, out: &mut Vec<Rec>) -> Result<()> {
        self.speaker(store, s.speaker_id, out)?;
        self.channel(store, s.channel_id, out)?;
        out.push(segment_rec(s, self.from, &self.pack));
        Ok(())
    }

    fn write(&mut self, recs: Vec<Rec>) -> Result<()> {
        if !recs.is_empty() {
            self.log.append(recs)?;
            self.last_write = crate::bot::now_ms();
        }
        Ok(())
    }
}

// ---------- publisher ----------

struct DayInfo {
    from: i64,
    to: i64,
    /// A summary is published and nothing changed since.
    summarized: bool,
}

struct ChannelsLog {
    gen: u64,
    log: LogFile,
    snapshot_len: u64,
}

impl ChannelsLog {
    fn file_name(gen: u64) -> String {
        format!("channels.{gen}.log")
    }
}

struct State {
    generation: String,
    days_log: LogFile,
    days: BTreeMap<String, DayInfo>,
    writers: HashMap<String, DayWriter>,
    channels: ChannelsLog,
    channels_rev: i64,
    speakers_rev: i64,
    last_retention: i64,
}

pub struct Publisher {
    cfg: Arc<Config>,
    store: Arc<Store>,
    state: Mutex<State>,
}

impl Publisher {
    /// Open the published logs, or build them from the database if they are
    /// missing or in an older format (e.g. the JSON indexes of earlier versions).
    pub fn open(cfg: Arc<Config>, store: Arc<Store>) -> Result<Self> {
        let public = cfg.public_dir();
        let meta: Option<serde_json::Value> =
            std::fs::read(public.join("meta.json")).ok().and_then(|b| serde_json::from_slice(&b).ok());
        let state = match meta.filter(|m| m["format"] == FORMAT) {
            Some(m) => match Self::load(&public, &store, &m) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("published logs unreadable ({e:#}); rebuilding them from the database");
                    Self::rebuild_logs(&cfg, &store)?
                }
            },
            None => {
                tracing::info!("building published logs from the database");
                Self::rebuild_logs(&cfg, &store)?
            }
        };
        let me = Self { cfg, store, state: Mutex::new(state) };
        me.write_meta(&me.state.lock().unwrap())?;
        Ok(me)
    }

    /// Discard and regenerate all published logs (CLI `republish`; run with the bot stopped).
    pub fn rebuild(cfg: Arc<Config>, store: Arc<Store>) -> Result<()> {
        let state = Self::rebuild_logs(&cfg, &store)?;
        let me = Self { cfg, store, state: Mutex::new(state) };
        let st = me.state.lock().unwrap();
        me.write_meta(&st)
    }

    fn load(public: &Path, store: &Store, meta: &serde_json::Value) -> Result<State> {
        let generation = meta["generation"].as_str().context("meta.json: generation")?.to_string();
        let (days_log, data) = LogFile::open(&public.join("days.log"))?;
        let mut days = BTreeMap::new();
        let (recs, _) = publog::parse(&data)?;
        for (ty, mut f) in recs {
            match ty {
                publog::DAY => {
                    let key = f.str()?;
                    let (from, to) = (f.u()? as i64, f.u()? as i64);
                    days.insert(key, DayInfo { from, to, summarized: false });
                }
                publog::SUMMARY => {
                    if let Some(d) = days.get_mut(&f.str()?) {
                        d.summarized = true;
                    }
                }
                publog::DAY_DELETED => {
                    days.remove(&f.str()?);
                }
                _ => {}
            }
        }
        let gen = meta["channelsGen"].as_u64().context("meta.json: channelsGen")?;
        let (log, _) = LogFile::open(&public.join(ChannelsLog::file_name(gen)))?;
        let snapshot_len = store.kv_get("channels_snapshot_len")?.unwrap_or(log.len as i64) as u64;
        Ok(State {
            generation,
            days_log,
            days,
            writers: HashMap::new(),
            channels: ChannelsLog { gen, log, snapshot_len },
            channels_rev: store.kv_get("channels_rev")?.unwrap_or(0),
            speakers_rev: store.kv_get("speakers_rev")?.unwrap_or(0),
            last_retention: 0,
        })
    }

    fn write_meta(&self, st: &State) -> Result<()> {
        let body = serde_json::json!({
            "format": FORMAT,
            "generation": st.generation,
            "server": self.cfg.mumble.address,
            "bot": self.cfg.mumble.username,
            "bitrate": self.cfg.recording.bitrate,
            "peaksPerSec": crate::transcode::PEAKS_PER_SEC,
            "days": "days.log",
            "channels": ChannelsLog::file_name(st.channels.gen),
            "channelsGen": st.channels.gen,
        });
        write_atomic(&self.cfg.public_dir().join("meta.json"), body.to_string().as_bytes())?;
        Ok(())
    }

    pub async fn run(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(self.cfg.recording.publish_interval_secs.max(1)));
        loop {
            tick.tick().await;
            let me = self.clone();
            match tokio::task::spawn_blocking(move || me.tick()).await {
                Ok(Err(e)) => tracing::error!("publish: {e:#}"),
                Err(e) => tracing::error!("publish task: {e}"),
                _ => {}
            }
        }
    }

    /// Append everything that changed in the database since the last call.
    pub fn tick(&self) -> Result<()> {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let st = &mut *guard;
        let now = crate::bot::now_ms();
        let today = day_key(now);
        self.writer(st, &today, true)?;
        self.publish_channels(st, &today)?;
        self.publish_speakers(st, &today)?;
        self.publish_segments(st)?;
        self.publish_intervals(st, "presence")?;
        self.publish_intervals(st, "uptime")?;
        self.publish_summaries(st, now)?;
        self.retention(st, now)?;
        self.compact_channels(st)?;
        st.writers.retain(|day, w| *day == today || now - w.last_write < WRITER_IDLE_MS);
        Ok(())
    }

    /// The writer for a day, creating its log (and its days.log entry) on first use.
    /// A newly started current day begins with everything still open at midnight.
    fn writer<'a>(&self, st: &'a mut State, day: &str, current: bool) -> Result<&'a mut DayWriter> {
        if !st.writers.contains_key(day) {
            let public = self.cfg.public_dir();
            let (from, to) = day_bounds(day).with_context(|| format!("bad day {day}"))?;
            let w = if DayWriter::path(&public, day).exists() && st.days.contains_key(day) {
                DayWriter::open(&public, day, from)?
            } else {
                let mut w = DayWriter::create(&public, day, from, to)?;
                if current {
                    let mut out = vec![];
                    for u in self.store.uptime_overlapping(from, from)? {
                        w.uptime_open(&u, &mut out);
                    }
                    for p in self.store.presence_overlapping(from, from)? {
                        w.presence_open(&self.store, &p, &mut out)?;
                    }
                    w.write(out)?;
                }
                if st.days.insert(day.to_string(), DayInfo { from, to, summarized: false }).is_none() {
                    st.days_log.append(vec![Rec::new(publog::DAY).str(day).u(from as u64).u(to as u64)])?;
                }
                w
            };
            st.writers.insert(day.to_string(), w);
        }
        Ok(st.writers.get_mut(day).unwrap())
    }

    fn publish_channels(&self, st: &mut State, today: &str) -> Result<()> {
        let changed = self.store.channels_since(st.channels_rev)?;
        let Some(max) = changed.iter().map(|c| c.1).max() else { return Ok(()) };
        let mut recs = vec![];
        let mut redefine = vec![];
        for (c, _) in &changed {
            recs.push(match c.removed_at {
                None => channel_rec(c),
                Some(_) => Rec::new(publog::CHANNEL_REMOVE).u(c.id as u64),
            });
            redefine.push(c.id);
        }
        st.channels.log.append(recs)?;
        // Today's log keeps the names current; past days keep the names they had.
        if let Some(w) = st.writers.get_mut(today) {
            let mut out = vec![];
            for id in redefine {
                if w.channels.remove(&id) {
                    w.channel(&self.store, id, &mut out)?;
                }
            }
            w.write(out)?;
        }
        st.channels_rev = max;
        self.store.kv_set("channels_rev", max)
    }

    fn publish_speakers(&self, st: &mut State, today: &str) -> Result<()> {
        let (ids, max) = self.store.speakers_since(st.speakers_rev)?;
        if ids.is_empty() {
            return Ok(());
        }
        if let Some(w) = st.writers.get_mut(today) {
            let mut out = vec![];
            for id in ids {
                if w.speakers.remove(&id) {
                    w.speaker(&self.store, id, &mut out)?;
                }
            }
            w.write(out)?;
        }
        st.speakers_rev = max;
        self.store.kv_set("speakers_rev", max)
    }

    fn publish_segments(&self, st: &mut State) -> Result<()> {
        loop {
            let segs = self.store.unpublished_segments(2000)?;
            if segs.is_empty() {
                return Ok(());
            }
            let mut by_day: BTreeMap<String, Vec<&PubSegment>> = BTreeMap::new();
            for s in &segs {
                by_day.entry(day_key(s.start_ms)).or_default().push(s);
            }
            for (day, list) in by_day {
                let w = self.writer(st, &day, false)?;
                let mut out = vec![];
                for s in list {
                    w.segment(&self.store, s, &mut out)?;
                }
                w.write(out)?;
                if let Some(d) = st.days.get_mut(&day) {
                    d.summarized = false;
                }
            }
            self.store.mark_published(&segs.iter().map(|s| s.id).collect::<Vec<_>>())?;
            if segs.len() < 2000 {
                return Ok(());
            }
        }
    }

    /// Presence intervals and bot sessions: the open goes to the day it
    /// started, the close to the day it ended (re-stating the open there if
    /// that day does not have it yet). An interval still open in a past day's
    /// log lasts until that day's end.
    fn publish_intervals(&self, st: &mut State, table: &str) -> Result<()> {
        let rows = self.store.unpublished_intervals(table)?;
        if rows.is_empty() {
            return Ok(());
        }
        let presence = table == "presence";
        let mut done = vec![];
        for r in &rows {
            let mut state = r.pub_state;
            if state & 1 == 0 {
                let w = self.writer(st, &day_key(r.start_ms), false)?;
                let mut out = vec![];
                if presence {
                    w.presence_open(&self.store, r, &mut out)?;
                } else {
                    w.uptime_open(r, &mut out);
                }
                w.write(out)?;
                state |= 1;
            }
            if let (Some(end), 0) = (r.end_ms, state & 2) {
                let w = self.writer(st, &day_key(end), false)?;
                let mut out = vec![];
                if presence {
                    w.presence_open(&self.store, r, &mut out)?;
                    out.push(Rec::new(publog::PRESENCE_CLOSE).u(r.id as u64).i(end - w.from));
                } else {
                    w.uptime_open(r, &mut out);
                    out.push(Rec::new(publog::UPTIME_CLOSE).u(r.id as u64).i(end - w.from));
                }
                w.write(out)?;
                state |= 2;
            }
            done.push((r.id, state));
        }
        self.store.set_interval_pub(table, &done)
    }

    fn publish_summaries(&self, st: &mut State, now: i64) -> Result<()> {
        let mut recs = vec![];
        for (day, d) in st.days.iter_mut().filter(|(_, d)| !d.summarized && now > d.to + SUMMARY_GRACE_MS) {
            let t = self.store.totals(d.from, d.to)?;
            if t.pending > 0 {
                continue;
            }
            recs.push(
                Rec::new(publog::SUMMARY)
                    .str(day)
                    .u(t.segments as u64)
                    .u(t.total_ms as u64)
                    .u(t.bytes as u64)
                    .u(t.raw_bytes as u64)
                    .u(t.first_ms as u64)
                    .u(t.last_ms as u64),
            );
            d.summarized = true;
        }
        st.days_log.append(recs)
    }

    /// Whole-day retention: a day older than the limit loses its segments, audio and log.
    fn retention(&self, st: &mut State, now: i64) -> Result<()> {
        let keep = self.cfg.recording.retention_days;
        if keep <= 0 || now - st.last_retention < 3_600_000 {
            return Ok(());
        }
        st.last_retention = now;
        let cutoff = now - keep * 86_400_000;
        let public = self.cfg.public_dir();
        let old: Vec<String> = st.days.iter().filter(|(_, d)| d.to <= cutoff).map(|(k, _)| k.clone()).collect();
        for day in &old {
            let d = &st.days[day];
            for p in self.store.delete_segments_in(d.from, d.to)? {
                if !self.store.path_in_use(&p)? {
                    let _ = std::fs::remove_file(public.join(&p));
                }
            }
            st.writers.remove(day);
            let _ = std::fs::remove_file(DayWriter::path(&public, day));
            st.days.remove(day);
            st.days_log.append(vec![Rec::new(publog::DAY_DELETED).str(day)])?;
        }
        if !old.is_empty() {
            tracing::info!("retention: removed {} day(s) up to {}", old.len(), old.last().unwrap());
        }
        self.store.prune_presence_before(cutoff)
    }

    /// Start a new channel log generation holding just the live channels once
    /// removals and edits have made the current one much larger than that.
    fn compact_channels(&self, st: &mut State) -> Result<()> {
        let (len, old_gen) = (st.channels.log.len, st.channels.gen);
        if len < COMPACT_MIN_BYTES || len < st.channels.snapshot_len * COMPACT_FACTOR {
            return Ok(());
        }
        let public = self.cfg.public_dir();
        let gen = old_gen + 1;
        let next = Self::channels_snapshot(&public, &self.store, gen)?;
        tracing::info!("channel log compacted: {len} -> {} bytes (generation {gen})", next.log.len);
        // Readers of the old generation are told to switch; the one before it is no longer referenced.
        st.channels.log.append(vec![Rec::new(publog::NEXT).u(gen)])?;
        let _ = std::fs::remove_file(public.join(ChannelsLog::file_name(old_gen.saturating_sub(1))));
        st.channels = next;
        self.store.kv_set("channels_snapshot_len", st.channels.snapshot_len as i64)?;
        self.write_meta(st)
    }

    fn channels_snapshot(public: &Path, store: &Store, gen: u64) -> Result<ChannelsLog> {
        let mut log =
            LogFile::create(&public.join(ChannelsLog::file_name(gen)), Rec::new(publog::HEADER).u(publog::KIND_CHANNELS as u64).u(gen))?;
        log.append(store.live_channels()?.iter().map(channel_rec).collect())?;
        let snapshot_len = log.len;
        Ok(ChannelsLog { gen, log, snapshot_len })
    }

    // ---------- full rebuild ----------

    /// Regenerate every published log from the database. Presence and sessions
    /// are swept day by day in start order, so this stays linear in the size of
    /// the history.
    fn rebuild_logs(cfg: &Config, store: &Store) -> Result<State> {
        let public = cfg.public_dir();
        remove_published(&public)?;
        let generation = format!("{:x}-{:x}", crate::bot::now_ms(), std::process::id());
        let mut days_log = LogFile::create(&public.join("days.log"), Rec::new(publog::HEADER).u(publog::KIND_DAYS as u64))?;
        let channels = Self::channels_snapshot(&public, store, 1)?;
        let rev = store.current_rev();
        let now = crate::bot::now_ms();
        let today = day_key(now);
        let mut days = BTreeMap::new();
        let mut presence: Vec<Interval> = vec![];
        let mut uptime: Vec<Interval> = vec![];
        let mut written = HashSet::new();

        let mut day = day_key(store.earliest_ms()?.unwrap_or(now).min(now));
        loop {
            let (from, to) = day_bounds(&day).with_context(|| format!("bad day {day}"))?;
            // Intervals overlapping this day: carried from earlier days, plus those starting today.
            presence.retain(|p| p.end_ms.is_none_or(|e| e > from));
            uptime.retain(|u| u.end_ms.is_none_or(|e| e > from));
            presence.extend(store.intervals_starting("presence", from, to)?);
            uptime.extend(store.intervals_starting("uptime", from, to)?);
            let mut segs = store.finished_segments(from, to)?;
            // Published segments keep their waveform peaks only in the day log:
            // take them from the log being replaced, or recompute from the audio.
            if segs.iter().any(|s| s.peaks.is_empty()) {
                let mut old = old_peaks(&DayWriter::path(&public, &day));
                for s in segs.iter_mut().filter(|s| s.peaks.is_empty()) {
                    s.peaks = match old.remove(&s.id) {
                        Some(p) => p,
                        None => peaks_from_audio(&public, s),
                    };
                }
            }
            written.insert(day.clone());

            if !(segs.is_empty() && presence.is_empty() && uptime.is_empty()) {
                let mut w = DayWriter::create(&public, &day, from, to)?;
                let mut out = vec![];
                for u in &uptime {
                    w.uptime_open(u, &mut out);
                    if let Some(e) = u.end_ms.filter(|&e| e < to) {
                        out.push(Rec::new(publog::UPTIME_CLOSE).u(u.id as u64).i(e - from));
                    }
                }
                for p in &presence {
                    w.presence_open(store, p, &mut out)?;
                    if let Some(e) = p.end_ms.filter(|&e| e < to) {
                        out.push(Rec::new(publog::PRESENCE_CLOSE).u(p.id as u64).i(e - from));
                    }
                }
                for s in &segs {
                    w.segment(store, s, &mut out)?;
                }
                w.write(out)?;
                let mut recs = vec![Rec::new(publog::DAY).str(&day).u(from as u64).u(to as u64)];
                let t = store.totals(from, to)?;
                let summarized = now > to + SUMMARY_GRACE_MS && t.pending == 0;
                if summarized {
                    recs.push(
                        Rec::new(publog::SUMMARY)
                            .str(&day)
                            .u(t.segments as u64)
                            .u(t.total_ms as u64)
                            .u(t.bytes as u64)
                            .u(t.raw_bytes as u64)
                            .u(t.first_ms as u64)
                            .u(t.last_ms as u64),
                    );
                }
                days_log.append(recs)?;
                days.insert(day.clone(), DayInfo { from, to, summarized });
            }
            if day >= today {
                break;
            }
            day = day_key(to);
        }
        // Day logs of days that no longer have any data.
        if let Ok(rd) = std::fs::read_dir(public.join("index")) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.strip_suffix(".log").is_none_or(|d| !written.contains(d)) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        store.mark_all_published()?;
        store.vacuum()?;
        store.kv_set("channels_rev", rev)?;
        store.kv_set("speakers_rev", rev)?;
        store.kv_set("channels_snapshot_len", channels.snapshot_len as i64)?;
        tracing::info!("published logs rebuilt: {} day(s)", days.len());
        Ok(State {
            generation,
            days_log,
            days,
            writers: HashMap::new(),
            channels,
            channels_rev: rev,
            speakers_rev: rev,
            last_retention: 0,
        })
    }
}

/// Waveform peaks of the segments in an existing day log, by segment id.
fn old_peaks(path: &Path) -> HashMap<i64, Vec<u8>> {
    let mut out = HashMap::new();
    let Ok(data) = std::fs::read(path) else { return out };
    let Ok((recs, _)) = publog::parse(&data) else { return out };
    for (ty, mut f) in recs {
        if ty != publog::SEGMENT {
            continue;
        }
        // id, speaker, channel, start, duration, target, status, path, offset, size, raw size, peaks
        let rec = (|| -> Result<(i64, Vec<u8>)> {
            let id = f.u()? as i64;
            for _ in 0..2 {
                f.u()?;
            }
            f.i()?;
            for _ in 0..3 {
                f.u()?;
            }
            f.bytes()?;
            f.opt()?;
            f.u()?;
            f.u()?;
            Ok((id, f.bytes()?.to_vec()))
        })();
        if let Ok((id, p)) = rec {
            out.insert(id, p);
        }
    }
    out
}

/// Recompute a segment's waveform peaks by decoding its stored audio.
fn peaks_from_audio(public: &Path, s: &PubSegment) -> Vec<u8> {
    let seg = crate::store::Segment {
        id: s.id,
        speaker_id: s.speaker_id,
        channel_id: s.channel_id,
        session: 0,
        target: s.target,
        start_ms: s.start_ms,
        end_ms: s.end_ms,
        status: if s.error { "error" } else { "ready" }.into(),
        path: Some(s.path.clone()),
        offset: s.offset,
        size: s.size,
    };
    match crate::codec::decode_segment(public, &seg) {
        Ok(pcm) => crate::transcode::peaks(&pcm),
        Err(e) => {
            tracing::warn!("segment {}: cannot recompute waveform: {e:#}", s.id);
            vec![]
        }
    }
}

/// Remove published index files (current and legacy formats); audio is kept.
/// Day logs are replaced one by one during a rebuild (their peaks are reused).
fn remove_published(public: &Path) -> Result<()> {
    for name in ["catalog.json", "channels.json", "speakers.json", "days.log", "meta.json"] {
        let _ = std::fs::remove_file(public.join(name));
    }
    let strip = |dir: &Path, pred: &dyn Fn(&str) -> bool| {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                if pred(&e.file_name().to_string_lossy()) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    };
    strip(public, &|n| n.starts_with("channels.") && n.ends_with(".log"));
    strip(&public.join("index"), &|n| n.ends_with(".json"));
    Ok(())
}
