//! Encrypted UDP voice channel to the Mumble server (legacy packet format).
//!
//! The server sends a client voice over UDP once it has learned the client's
//! UDP address (from any valid encrypted packet) and the client's "UDP flag" is
//! set; it falls back to the TCP tunnel when the client sends anything through
//! the tunnel. A valid voice packet over UDP sets the flag again.

use super::crypt::CryptState;
use super::varint;
use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

/// Legacy UDP message type for pings (header byte = type << 5).
const TYPE_PING: u8 = 1;

pub enum UdpEvent {
    /// Decrypted voice packet (same format as a UDPTunnel payload).
    Voice(Vec<u8>),
    /// Echo of one of our pings carrying its send timestamp (ms).
    Pong(u64),
    /// A datagram that failed authentication/decryption.
    DecryptFailed,
}

pub struct Udp {
    sock: Arc<UdpSocket>,
    crypt: Arc<Mutex<CryptState>>,
    task: tokio::task::JoinHandle<()>,
}

/// A legacy ping message; the server echoes it back unchanged.
pub fn ping_packet(timestamp_ms: u64) -> Vec<u8> {
    let mut p = vec![TYPE_PING << 5];
    varint::write(&mut p, timestamp_ms);
    p
}

/// A one-frame silent Opus voice packet with the terminator bit. Sent over
/// UDP to make the server route our incoming voice via UDP; the bot is
/// self-muted, so the server does not relay it to anyone.
pub fn udp_activation_packet(sequence: u64) -> Vec<u8> {
    super::build_client_audio(0, sequence, &[0xF8, 0xFF, 0xFE], true)
}

/// Requested kernel receive buffer: absorbs seconds of voice from many
/// concurrent speakers if the process is briefly busy (OS defaults can be
/// as small as 64 KB, i.e. well under 100 ms at high load).
const RECV_BUFFER: usize = 8 << 20;

fn bind_socket(server: SocketAddr) -> Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let domain = if server.is_ipv4() { Domain::IPV4 } else { Domain::IPV6 };
    let s = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP)).context("create UDP socket")?;
    if let Err(e) = s.set_recv_buffer_size(RECV_BUFFER) {
        tracing::warn!("UDP: cannot enlarge receive buffer: {e}");
    }
    let any: SocketAddr = if server.is_ipv4() { "0.0.0.0:0".parse()? } else { "[::]:0".parse()? };
    s.bind(&any.into()).context("bind UDP socket")?;
    s.connect(&server.into()).with_context(|| format!("UDP connect {server}"))?;
    s.set_nonblocking(true)?;
    let got = s.recv_buffer_size().unwrap_or(0);
    tracing::debug!("UDP: receive buffer {} KB", got / 1024);
    if got < RECV_BUFFER / 2 {
        // Linux caps SO_RCVBUF at net.core.rmem_max (often ~200 KB: well under a
        // second of voice for a very busy server).
        tracing::warn!(
            "UDP: kernel granted a {} KB receive buffer ({} KB requested); for very busy servers \
             raise the limit, e.g. 'sysctl -w net.core.rmem_max={}'",
            got / 1024,
            RECV_BUFFER / 1024,
            RECV_BUFFER
        );
    }
    Ok(UdpSocket::from_std(s.into())?)
}

impl Udp {
    pub async fn connect(server: SocketAddr, crypt: CryptState, events: mpsc::UnboundedSender<UdpEvent>) -> Result<Self> {
        let sock = Arc::new(bind_socket(server)?);
        let crypt = Arc::new(Mutex::new(crypt));
        let (rs, rc) = (sock.clone(), crypt.clone());
        let task = tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                let n = match rs.recv(&mut buf).await {
                    Ok(n) => n,
                    // ICMP port unreachable etc. surface as errors on connected sockets: keep going.
                    Err(_) => {
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        continue;
                    }
                };
                let plain = rc.lock().unwrap_or_else(|e| e.into_inner()).decrypt(&buf[..n]);
                let ev = match plain {
                    None => UdpEvent::DecryptFailed,
                    Some(p) if p.first().map(|b| b >> 5) == Some(TYPE_PING) => {
                        UdpEvent::Pong(varint::Reader::new(&p[1..]).varint().unwrap_or(0) as u64)
                    }
                    Some(p) if !p.is_empty() => UdpEvent::Voice(p),
                    Some(_) => continue,
                };
                if events.send(ev).is_err() {
                    break;
                }
            }
        });
        Ok(Self { sock, crypt, task })
    }

    pub async fn send(&self, plain: &[u8]) -> Result<()> {
        let enc = self.crypt.lock().unwrap_or_else(|e| e.into_inner()).encrypt(plain);
        self.sock.send(&enc).await?;
        Ok(())
    }

    pub fn crypt(&self) -> std::sync::MutexGuard<'_, CryptState> {
        self.crypt.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for Udp {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// UDP voice link policy on top of [`Udp`]:
///
/// 1. Probe: send an encrypted ping, and tell the server to keep using the TCP
///    tunnel (any tunnel message clears its UDP flag) — nothing is lost while
///    UDP is unproven.
/// 2. Activate: on the first pong (UDP works both ways) send one muted voice
///    packet over UDP, which makes the server route our voice via UDP.
/// 3. Fall back: if pongs stop, send a tunnel ping so the server returns to TCP;
///    re-activate when pongs come back.
/// 4. Resync: if packets stop decrypting, ask the server for a new nonce.
pub struct VoiceLink {
    udp: Udp,
    active: bool,
    last_pong: Option<i64>,
    last_decrypt: i64,
    failed: u32,
    last_resync_request: i64,
    activation_seq: u64,
    last_activation: i64,
    /// Smoothed UDP round-trip time (ms).
    pub rtt_ms: Option<f64>,
    pub udp_packets: u32,
}

const PONG_TIMEOUT_MS: i64 = 15_000;
/// Echoes of pings older than this (e.g. queued during an outage) don't count as RTT samples.
const MAX_RTT_SAMPLE_MS: i64 = 5_000;
const REACTIVATE_EVERY_MS: i64 = 5_000;
const RESYNC_AFTER_MS: i64 = 1_500;
const FAIL_FAST_MS: i64 = 1_000;

impl VoiceLink {
    pub async fn start(conn: &mut super::Connection, crypt: CryptState, events: mpsc::UnboundedSender<UdpEvent>, now: i64) -> Result<Self> {
        let udp = Udp::connect(conn.peer, crypt, events).await?;
        let link = Self {
            udp,
            active: false,
            last_pong: None,
            last_decrypt: now,
            failed: 0,
            last_resync_request: now,
            activation_seq: 0,
            last_activation: 0,
            rtt_ms: None,
            udp_packets: 0,
        };
        link.udp.send(&ping_packet(now as u64)).await?;
        conn.send_raw(super::kind::UDP_TUNNEL, &ping_packet(now as u64)).await?;
        tracing::info!("UDP: probing {}", conn.peer);
        Ok(link)
    }

    pub fn active(&self) -> bool {
        self.active
    }

    pub async fn on_event(&mut self, conn: &mut super::Connection, ev: &UdpEvent, now: i64) -> Result<()> {
        match ev {
            UdpEvent::Voice(_) => {
                self.last_decrypt = now;
                self.failed = 0;
                self.udp_packets = self.udp_packets.wrapping_add(1);
            }
            UdpEvent::Pong(sent) => {
                self.last_decrypt = now;
                self.failed = 0;
                self.last_pong = Some(now);
                let rtt = (now - *sent as i64).max(0);
                if rtt <= MAX_RTT_SAMPLE_MS {
                    let rtt = rtt as f64;
                    self.rtt_ms = Some(self.rtt_ms.map_or(rtt, |r| r * 0.8 + rtt * 0.2));
                }
                if !self.active {
                    self.activate(now).await?;
                    self.active = true;
                    tracing::info!("UDP: voice now arrives over UDP (rtt {} ms)", self.rtt_ms.map_or(rtt as f64, |r| r).round());
                }
            }
            UdpEvent::DecryptFailed => {
                self.failed += 1;
                // Voice is arriving but can't be decrypted: don't wait for the
                // pong timeout, move the server to TCP right away and resync.
                if self.active && now - self.last_decrypt > FAIL_FAST_MS {
                    self.active = false;
                    conn.send_raw(super::kind::UDP_TUNNEL, &ping_packet(now as u64)).await?;
                    tracing::warn!("UDP: packets no longer decrypt, voice falls back to TCP");
                }
                if now - self.last_decrypt > FAIL_FAST_MS && now - self.last_resync_request > RESYNC_AFTER_MS {
                    self.last_resync_request = now;
                    conn.send(super::kind::CRYPT_SETUP, &super::pb::CryptSetup::default()).await?;
                    tracing::info!("UDP: requesting nonce resync");
                }
            }
        }
        Ok(())
    }

    async fn activate(&mut self, now: i64) -> Result<()> {
        self.activation_seq += 1;
        self.last_activation = now;
        self.udp.send(&udp_activation_packet(self.activation_seq)).await
    }

    /// Voice arrived through the TCP tunnel. If UDP is supposed to be active the
    /// server missed (or overrode) our activation, e.g. a fallback ping queued
    /// during an outage was processed after it: assert UDP again.
    pub async fn on_tcp_voice(&mut self, now: i64) -> Result<()> {
        if self.active && now - self.last_activation > REACTIVATE_EVERY_MS {
            self.activate(now).await?;
            tracing::debug!("UDP: voice still arriving over TCP, re-activating UDP");
        }
        Ok(())
    }

    /// Call every few seconds: keepalive ping, fallback and resync checks.
    pub async fn tick(&mut self, conn: &mut super::Connection, now: i64) -> Result<()> {
        self.udp.send(&ping_packet(now as u64)).await?;
        let silent_for = now - self.last_pong.unwrap_or(now);
        if self.active && silent_for > PONG_TIMEOUT_MS {
            self.active = false;
            conn.send_raw(super::kind::UDP_TUNNEL, &ping_packet(now as u64)).await?;
            tracing::warn!("UDP: no pong for {} s, voice falls back to TCP", silent_for / 1000);
        }
        if self.failed > 0 && now - self.last_decrypt > RESYNC_AFTER_MS && now - self.last_resync_request > RESYNC_AFTER_MS {
            self.last_resync_request = now;
            conn.send(super::kind::CRYPT_SETUP, &super::pb::CryptSetup::default()).await?;
            tracing::info!("UDP: {} undecryptable packets, requesting nonce resync", self.failed);
        }
        Ok(())
    }

    /// Handle a CryptSetup after the initial key exchange: a nonce resync from
    /// the server, or the server asking for our nonce.
    pub async fn on_crypt_setup(&mut self, conn: &mut super::Connection, m: &super::pb::CryptSetup) -> Result<()> {
        match &m.server_nonce {
            Some(n) if m.key.is_none() => {
                self.udp.crypt().set_decrypt_iv(n);
                self.failed = 0;
            }
            None if m.key.is_none() && m.client_nonce.is_none() => {
                let iv = self.udp.crypt().encrypt_iv();
                let reply = super::pb::CryptSetup { client_nonce: Some(iv.to_vec()), ..Default::default() };
                conn.send(super::kind::CRYPT_SETUP, &reply).await?;
            }
            _ => {}
        }
        Ok(())
    }

    /// (lost, late) packet counts on the UDP path.
    pub fn loss(&self) -> (u32, u32) {
        let c = self.udp.crypt();
        (c.lost, c.late)
    }

    /// Fill the UDP statistics fields of a TCP ping.
    pub fn fill_ping(&self, p: &mut super::pb::Ping) {
        let c = self.udp.crypt();
        p.good = Some(c.good);
        p.late = Some(c.late);
        p.lost = Some(c.lost);
        p.resync = Some(c.resync);
        p.udp_packets = Some(self.udp_packets);
        if let Some(r) = self.rtt_ms {
            p.udp_ping_avg = Some(r as f32);
        }
    }
}
