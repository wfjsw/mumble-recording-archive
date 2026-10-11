use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub mumble: MumbleConfig,
    pub recording: RecordingConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MumbleConfig {
    /// host:port of the Mumble server.
    pub address: String,
    pub username: String,
    pub password: String,
    /// Access tokens (ACL groups).
    pub tokens: Vec<String>,
    /// Verify the server certificate against public roots. Most Mumble servers are self-signed.
    pub tls_verify: bool,
    /// PEM cert/key for the bot identity. Generated on first start if missing
    /// (relative paths are resolved against the data dir).
    pub cert_file: String,
    pub key_file: String,
    /// Optional channel path (e.g. "Root/Lobby" or "Lobby") the bot moves into after connecting.
    pub home_channel: String,
    /// Set the "recording" flag so clients display a recording indicator next to the bot.
    pub announce_recording: bool,
    /// Comment shown on the bot user.
    pub comment: String,
    pub reconnect_delay_secs: u64,
    /// Receive voice over encrypted UDP when it works, falling back to the TCP
    /// tunnel automatically. Disable to always use TCP.
    pub udp: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RecordingConfig {
    /// Private state: database, bot certificate, temp files.
    pub data_dir: PathBuf,
    /// Published files (indexes + audio) for the static web UI; relative to data_dir.
    /// Point the web server at this directory — it never contains the DB or keys.
    pub public_dir: String,
    /// Target bitrate for the VBR speech re-encode (libopus). 12k is compact and
    /// clearly intelligible; 16k–24k sounds closer to the original.
    pub bitrate: String,
    /// Silence (ms) after which a talk burst is closed if no terminator was received.
    pub gap_ms: i64,
    /// Bursts longer than this are split into multiple contiguous segments.
    pub max_segment_secs: i64,
    /// Bursts shorter than this are discarded (stray terminator packets etc).
    pub min_segment_ms: i64,
    /// Number of concurrent transcode jobs (0 = half the CPU cores).
    pub transcode_workers: usize,
    /// Keep the original pass-through Opus stream next to the re-encoded file.
    pub keep_raw: bool,
    /// Delete recordings older than this many days (0 = keep forever).
    pub retention_days: i64,
    /// Drop silence inside talk bursts: bursts are split at pauses and only the
    /// speech regions are stored (at their exact time), so silence costs nothing.
    pub trim_silence: bool,
    /// How often changed static index files are regenerated (seconds).
    pub publish_interval_secs: u64,
    /// Peak level below which audio counts as silence.
    pub silence_threshold_db: f32,
    /// Only pauses at least this long are removed.
    pub min_silence_ms: i64,
    /// Audio kept around each speech region so word onsets/tails are not clipped.
    pub silence_pad_ms: i64,
}


impl Default for MumbleConfig {
    fn default() -> Self {
        Self {
            address: "127.0.0.1:64738".into(),
            username: "ArchiveBot".into(),
            password: String::new(),
            tokens: vec![],
            tls_verify: false,
            cert_file: "bot-cert.pem".into(),
            key_file: "bot-key.pem".into(),
            home_channel: String::new(),
            announce_recording: true,
            comment: "This bot records voice in this server.".into(),
            reconnect_delay_secs: 5,
            udp: true,
        }
    }
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("data"),
            public_dir: "public".into(),
            bitrate: "12k".into(),
            gap_ms: 1500,
            max_segment_secs: 300,
            min_segment_ms: 60,
            transcode_workers: 0,
            keep_raw: false,
            retention_days: 0,
            trim_silence: true,
            publish_interval_secs: 10,
            silence_threshold_db: -45.0,
            min_silence_ms: 500,
            silence_pad_ms: 150,
        }
    }
}


impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            tracing::warn!("config {} not found, using defaults", path.display());
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let cfg: Config = toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        Ok(cfg)
    }

    /// Target bitrate in bits/s parsed from e.g. "12k" / "12000".
    pub fn target_bps(&self) -> i64 {
        let b = self.recording.bitrate.trim().to_ascii_lowercase();
        match b.strip_suffix('k') {
            Some(k) => (k.parse::<f64>().unwrap_or(12.0) * 1000.0) as i64,
            None => b.parse().unwrap_or(12_000),
        }
    }

    /// Directory served by the static web server (UI data + audio).
    pub fn public_dir(&self) -> PathBuf {
        let p = Path::new(&self.recording.public_dir);
        if p.is_absolute() { p.to_path_buf() } else { self.recording.data_dir.join(p) }
    }

    pub fn data_path(&self, p: &str) -> PathBuf {
        let p = Path::new(p);
        if p.is_absolute() { p.to_path_buf() } else { self.recording.data_dir.join(p) }
    }
}
