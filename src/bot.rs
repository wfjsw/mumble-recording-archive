//! The recording bot: keeps a Mumble session alive, mirrors server state into
//! the archive (channels, speakers, presence) and cuts incoming voice into
//! per-speaker segments stored as pass-through Ogg Opus.
//!
//! The bot is passive: it records whatever voice the server delivers to it
//! (its own channel, whispers/shouts targeting it, channels an admin made it
//! listen to) and never subscribes to anything by itself.

use crate::config::Config;
use crate::live::Live;
use crate::mumble::{self, kind, pb, AudioPacket, Connection, Identity};
use crate::ogg::OggOpusWriter;
use crate::opus;
use crate::store::Store;
use anyhow::{bail, Result};
use prost::Message;
use std::collections::HashMap;
use std::fs::File;
use std::io::BufWriter;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

pub struct Bot {
    pub cfg: Arc<Config>,
    pub store: Arc<Store>,
    pub live: Arc<Live>,
    pub transcode: mpsc::UnboundedSender<i64>,
    pub identity: Identity,
}

struct ChannelInfo {
    internal: i64,
    name: String,
    parent: Option<u32>,
    position: i32,
}

#[derive(Default)]
struct UserInfo {
    name: String,
    user_id: Option<u32>,
    hash: Option<String>,
    channel: u32,
    speaker_id: i64,
    presence: Option<(i64, i64)>, // (presence row, internal channel)
    flags: [bool; 5], // self_mute, mute, suppress, self_deaf, deaf
}

struct Stream {
    seg_id: i64,
    speaker: i64,
    channel: i64,
    target: u8,
    start_ms: i64,
    samples: i64,
    last_seq: i64,
    last_step: i64,
    last_arrival: i64,
    last_db_update: i64,
    raw_path: std::path::PathBuf,
    writer: OggOpusWriter<BufWriter<File>>,
}

#[derive(Default)]
struct State {
    self_session: Option<u32>,
    self_channel: Option<u32>,
    synced: bool,
    channels: HashMap<u32, ChannelInfo>,
    users: HashMap<u32, UserInfo>,
    streams: HashMap<u32, Stream>,
    uptime: Option<i64>,
    last_rx: i64,
    warned_codec: bool,
}

enum SplitReason {
    None,
    Discontinuity,
    MaxLength,
}

impl Bot {
    pub async fn run(self: Arc<Self>) {
        loop {
            let started = now_ms();
            match self.session().await {
                Ok(()) => tracing::info!("disconnected"),
                Err(e) => tracing::warn!("mumble session ended: {e:#}"),
            }
            // Back off faster if the last session was healthy for a while.
            let delay = if now_ms() - started > 60_000 { 1 } else { self.cfg.mumble.reconnect_delay_secs.max(1) };
            tokio::time::sleep(Duration::from_secs(delay)).await;
        }
    }

    async fn session(&self) -> Result<()> {
        let m = &self.cfg.mumble;
        tracing::info!("connecting to {} as {}", m.address, m.username);
        let mut conn = Connection::connect(&m.address, m.tls_verify, &self.identity).await?;
        conn.handshake(&m.username, &m.password, &m.tokens, true).await?;

        let mut st = State { last_rx: now_ms(), ..Default::default() };
        let mut ping = tokio::time::interval(Duration::from_secs(10));
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        let mut heartbeat = tokio::time::interval(Duration::from_secs(10));

        let res: Result<()> = async {
            loop {
                tokio::select! {
                    f = conn.recv() => {
                        let f = f?;
                        st.last_rx = now_ms();
                        self.handle(&mut conn, &mut st, f).await?;
                    }
                    _ = ping.tick() => {
                        conn.send(kind::PING, &pb::Ping { timestamp: Some(now_ms() as u64), ..Default::default() }).await?;
                    }
                    _ = heartbeat.tick() => {
                        if let Some(id) = st.uptime { self.store.touch_uptime(id, now_ms())?; }
                    }
                    _ = tick.tick() => {
                        let now = now_ms();
                        if now - st.last_rx > 40_000 { bail!("server timed out"); }
                        self.expire_streams(&mut st, now);
                    }
                }
            }
        }
        .await;

        self.teardown(&mut st);
        res
    }

    fn teardown(&self, st: &mut State) {
        let now = now_ms();
        let sessions: Vec<u32> = st.streams.keys().copied().collect();
        for s in sessions {
            self.finalize(st, s);
        }
        for u in st.users.values_mut() {
            if let Some((p, _)) = u.presence.take() {
                let _ = self.store.close_presence(p, now);
            }
        }
        if let Some(id) = st.uptime {
            let _ = self.store.touch_uptime(id, now);
        }
    }

    async fn handle(&self, conn: &mut Connection, st: &mut State, f: mumble::Frame) -> Result<()> {
        let now = now_ms();
        match f.kind {
            kind::UDP_TUNNEL => {
                if let Some(pkt) = mumble::parse_server_audio(&f.payload) {
                    self.on_audio(st, pkt, now);
                }
            }
            kind::SERVER_SYNC => {
                let m = pb::ServerSync::decode(&f.payload[..])?;
                let me = m.session.unwrap_or(0);
                st.self_session = Some(me);
                st.synced = true;
                if let Some(u) = st.users.remove(&me) {
                    st.self_channel = Some(u.channel);
                    self.store.forget_speaker_if_unused(u.speaker_id)?;
                    self.live.touch_speakers();
                }
                let alive: Vec<i64> = st.channels.values().map(|c| c.internal).collect();
                self.store.mark_channels_removed_except(&alive, now)?;
                self.live.touch_channels();
                st.uptime = Some(self.store.start_uptime(now, &self.cfg.mumble.address)?);
                tracing::info!("synced: session {me}, {} channels, {} users", st.channels.len(), st.users.len());

                let mut us = pb::UserState {
                    session: Some(me),
                    self_mute: Some(true),
                    self_deaf: Some(false),
                    ..Default::default()
                };
                if self.cfg.mumble.announce_recording {
                    us.recording = Some(true);
                }
                if !self.cfg.mumble.comment.is_empty() {
                    us.comment = Some(self.cfg.mumble.comment.clone());
                }
                conn.send(kind::USER_STATE, &us).await?;
                if let Some(target) = self.find_channel(st, &self.cfg.mumble.home_channel) {
                    conn.send(kind::USER_STATE, &pb::UserState { session: Some(me), channel_id: Some(target), ..Default::default() })
                        .await?;
                }
            }
            kind::CHANNEL_STATE => {
                let m = pb::ChannelState::decode(&f.payload[..])?;
                let Some(id) = m.channel_id else { return Ok(()) };
                let is_new = !st.channels.contains_key(&id);
                let (name, parent, position) = match st.channels.get(&id) {
                    Some(c) => (m.name.clone().unwrap_or(c.name.clone()), m.parent.or(c.parent), m.position.unwrap_or(c.position)),
                    None => (m.name.clone().unwrap_or_default(), m.parent, m.position.unwrap_or(0)),
                };
                let parent_internal = parent.and_then(|p| st.channels.get(&p)).map(|c| c.internal);
                let internal = if is_new {
                    self.store.resolve_channel(id, &name, parent_internal, position as i64, now)?
                } else {
                    let c = &st.channels[&id];
                    self.store.update_channel(c.internal, &name, parent_internal, position as i64)?;
                    c.internal
                };
                st.channels.insert(id, ChannelInfo { internal, name, parent, position });
                self.live.touch_channels();
            }
            kind::CHANNEL_REMOVE => {
                let m = pb::ChannelRemove::decode(&f.payload[..])?;
                if let Some(c) = st.channels.remove(&m.channel_id) {
                    self.store.remove_channel(c.internal, now)?;
                    self.live.touch_channels();
                }
            }
            kind::USER_STATE => {
                let m = pb::UserState::decode(&f.payload[..])?;
                self.on_user_state(st, m, now)?;
            }
            kind::USER_REMOVE => {
                let m = pb::UserRemove::decode(&f.payload[..])?;
                if st.self_session == Some(m.session) {
                    bail!("removed from server: {}", m.reason.unwrap_or_default());
                }
                self.finalize(st, m.session);
                if let Some(u) = st.users.remove(&m.session) {
                    if let Some((p, _)) = u.presence {
                        self.store.close_presence(p, now)?;
                    }
                }
            }
            kind::REJECT => {
                let m = pb::Reject::decode(&f.payload[..])?;
                bail!("rejected by server: {}", m.reason.unwrap_or_else(|| format!("type {:?}", m.r#type)));
            }
            kind::PERMISSION_DENIED => {
                let m = pb::PermissionDenied::decode(&f.payload[..])?;
                tracing::warn!(
                    "permission denied: {} (type {:?}, channel {:?})",
                    m.reason.unwrap_or_default(),
                    m.r#type,
                    m.channel_id
                );
            }
            kind::CODEC_VERSION => {
                let m = pb::CodecVersion::decode(&f.payload[..])?;
                if m.opus == Some(false) {
                    tracing::warn!("server does not enable Opus; only Opus voice is recorded");
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn find_channel(&self, st: &State, path: &str) -> Option<u32> {
        let path = path.trim().trim_matches('/');
        if path.is_empty() {
            return None;
        }
        let full = |id: u32| {
            let mut parts = vec![];
            let mut cur = Some(id);
            while let Some(c) = cur.and_then(|i| st.channels.get(&i)) {
                parts.push(c.name.as_str());
                cur = c.parent;
            }
            parts.reverse();
            parts
        };
        st.channels.keys().copied().find(|&id| {
            let p = full(id);
            p.join("/").eq_ignore_ascii_case(path) || p[1.min(p.len())..].join("/").eq_ignore_ascii_case(path)
        })
    }

    fn speaker_key(u: &UserInfo) -> String {
        if let Some(id) = u.user_id {
            format!("uid:{id}")
        } else if let Some(h) = u.hash.as_deref().filter(|h| !h.is_empty()) {
            format!("cert:{h}")
        } else {
            format!("name:{}", u.name)
        }
    }

    fn on_user_state(&self, st: &mut State, m: pb::UserState, now: i64) -> Result<()> {
        let Some(session) = m.session else { return Ok(()) };
        if st.self_session == Some(session) {
            if let Some(c) = m.channel_id {
                st.self_channel = Some(c);
            }
            return Ok(());
        }
        let is_new = !st.users.contains_key(&session);
        let u = st.users.entry(session).or_default();
        let ident_before = (u.name.clone(), u.user_id, u.hash.clone());
        let channel_before = u.channel;
        if let Some(n) = m.name {
            u.name = n;
        }
        if let Some(id) = m.user_id {
            // u32::MAX means "unregistered" in some server versions
            u.user_id = (id != u32::MAX).then_some(id);
        }
        if let Some(h) = m.hash {
            u.hash = Some(h);
        }
        if let Some(c) = m.channel_id {
            u.channel = c;
        }
        for (i, v) in [m.self_mute, m.mute, m.suppress, m.self_deaf, m.deaf].into_iter().enumerate() {
            if let Some(v) = v {
                u.flags[i] = v;
            }
        }

        if is_new || ident_before != (u.name.clone(), u.user_id, u.hash.clone()) {
            self.live.touch_speakers();
            u.speaker_id = self.store.upsert_speaker(
                &Self::speaker_key(u),
                &u.name,
                u.user_id.map(|x| x as i64),
                u.hash.as_deref(),
                now,
            )?;
        }
        let channel_internal = st.channels.get(&u.channel).map(|c| c.internal);
        if is_new || channel_before != u.channel || u.presence.map(|p| p.1) != channel_internal {
            if let Some((p, _)) = u.presence.take() {
                self.store.close_presence(p, now)?;
            }
            self.live.touch_day(now);
            if let Some(ci) = channel_internal {
                let p = self.store.open_presence(u.speaker_id, ci, session, now)?;
                u.presence = Some((p, ci));
            }
        }
        Ok(())
    }

    fn on_audio(&self, st: &mut State, pkt: AudioPacket, now: i64) {
        if pkt.codec != mumble::CODEC_OPUS {
            if !st.warned_codec {
                st.warned_codec = true;
                tracing::warn!("ignoring non-Opus voice (codec {})", pkt.codec);
            }
            return;
        }
        if st.self_session == Some(pkt.session) {
            return;
        }
        let Some(u) = st.users.get(&pkt.session) else { return };
        let Some(channel) = st.channels.get(&u.channel).map(|c| c.internal) else { return };
        let speaker = u.speaker_id;
        let samples = if pkt.opus.is_empty() {
            0
        } else {
            match opus::packet_samples(&pkt.opus) {
                Some(n) => n,
                None => return,
            }
        };
        let rc = &self.cfg.recording;
        let gap = rc.gap_ms;

        let mut contiguous_start = None;
        let reason = match st.streams.get(&pkt.session) {
            None => SplitReason::None,
            Some(s) => {
                let expected_end = s.start_ms + s.samples / opus::SAMPLES_PER_MS;
                // Channel/target/identity change, sequence reset, or a long pause: new burst.
                if s.channel != channel
                    || s.target != pkt.target
                    || s.speaker != speaker
                    || pkt.sequence < s.last_seq
                    || now - expected_end > gap
                {
                    SplitReason::Discontinuity
                } else if s.samples >= rc.max_segment_secs.max(10) * opus::SAMPLE_RATE {
                    contiguous_start = Some(expected_end);
                    SplitReason::MaxLength
                } else {
                    SplitReason::None
                }
            }
        };
        if !matches!(reason, SplitReason::None) {
            self.finalize(st, pkt.session);
        }
        if samples == 0 {
            if pkt.terminator {
                self.finalize(st, pkt.session);
            }
            return;
        }

        #[allow(clippy::map_entry)] // opening a stream does fallible I/O first
        if !st.streams.contains_key(&pkt.session) {
            let start = contiguous_start.unwrap_or(now - samples / opus::SAMPLES_PER_MS);
            match self.open_stream(speaker, channel, pkt.session, pkt.target, start) {
                Ok(s) => {
                    st.streams.insert(pkt.session, s);
                }
                Err(e) => {
                    tracing::error!("cannot open segment: {e:#}");
                    return;
                }
            }
        }
        let s = st.streams.get_mut(&pkt.session).unwrap();

        // Fill sender-side gaps (lost packets / DTX) with silence, as indicated by the sequence number.
        if s.samples > 0 {
            let expected_seq = s.last_seq + s.last_step;
            if pkt.sequence > expected_seq {
                let missing = (pkt.sequence - expected_seq) * 480;
                if missing <= gap * opus::SAMPLES_PER_MS {
                    for frame in opus::silence(missing) {
                        let n = opus::packet_samples(frame).unwrap();
                        if s.writer.write_packet(frame, n).is_ok() {
                            s.samples += n;
                        }
                    }
                }
            }
        }
        if let Err(e) = s.writer.write_packet(&pkt.opus, samples) {
            tracing::error!("write segment {}: {e}", s.seg_id);
        }
        s.samples += samples;
        s.last_seq = pkt.sequence;
        s.last_step = (samples / 480).max(1);
        s.last_arrival = now;

        if pkt.terminator {
            self.finalize(st, pkt.session);
        }
    }

    fn open_stream(&self, speaker: i64, channel: i64, session: u32, target: u8, start: i64) -> Result<Stream> {
        let rel = format!("raw/{start}-{session}.opus");
        let path = self.cfg.public_dir().join(&rel);
        std::fs::create_dir_all(path.parent().unwrap())?;
        let seg_id = self.store.insert_segment(speaker, channel, session, target, start, &rel)?;
        let f = BufWriter::new(File::create(&path)?);
        let writer = OggOpusWriter::new(f, seg_id as u32, "mumble-archive passthrough")?;
        Ok(Stream {
            seg_id,
            speaker,
            channel,
            target,
            start_ms: start,
            samples: 0,
            last_seq: 0,
            last_step: 0,
            last_arrival: now_ms(),
            last_db_update: now_ms(),
            raw_path: path,
            writer,
        })
    }

    fn expire_streams(&self, st: &mut State, now: i64) {
        let gap = self.cfg.recording.gap_ms;
        let expired: Vec<u32> = st.streams.iter().filter(|(_, s)| now - s.last_arrival > gap).map(|(k, _)| *k).collect();
        for s in expired {
            self.finalize(st, s);
        }
        for s in st.streams.values_mut() {
            if now - s.last_db_update >= 1000 {
                s.last_db_update = now;
                let _ = s.writer.flush();
                let _ = self.store.update_segment_length(s.seg_id, s.start_ms + s.samples / opus::SAMPLES_PER_MS, s.samples);
            }
        }
    }

    fn finalize(&self, st: &mut State, session: u32) {
        let Some(s) = st.streams.remove(&session) else { return };
        let seg_id = s.seg_id;
        let dur_ms = s.samples / opus::SAMPLES_PER_MS;
        let end = s.start_ms + dur_ms;
        let path = s.raw_path.clone();
        let res = s.writer.finish().map(drop);
        if let Err(e) = res {
            tracing::error!("finish segment {seg_id}: {e}");
        }
        if dur_ms < self.cfg.recording.min_segment_ms {
            let _ = std::fs::remove_file(&path);
            let _ = self.store.delete_segment(seg_id);
            return;
        }
        let size = std::fs::metadata(&path).map(|m| m.len() as i64).unwrap_or(0);
        if let Err(e) = self.store.finish_segment(seg_id, end, s.samples, size) {
            tracing::error!("store segment {seg_id}: {e}");
            return;
        }
        let _ = self.store.touch_speaker(s.speaker, end);
        self.live.touch_speakers();
        self.live.touch_day(s.start_ms);
        let _ = self.transcode.send(seg_id);
    }
}
