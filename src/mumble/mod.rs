//! Minimal Mumble client protocol: TLS control channel with protobuf messages,
//! voice received through the TCP tunnel (legacy, pre-1.5 voice packet format).

pub mod varint;

#[allow(clippy::all)]
pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/mumble_proto.rs"));
}

use anyhow::{anyhow, bail, Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use rustls_pki_types::pem::PemObject;
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::client::TlsStream;

pub mod kind {
    pub const VERSION: u16 = 0;
    pub const UDP_TUNNEL: u16 = 1;
    pub const AUTHENTICATE: u16 = 2;
    pub const PING: u16 = 3;
    pub const REJECT: u16 = 4;
    pub const SERVER_SYNC: u16 = 5;
    pub const CHANNEL_REMOVE: u16 = 6;
    pub const CHANNEL_STATE: u16 = 7;
    pub const USER_REMOVE: u16 = 8;
    pub const USER_STATE: u16 = 9;
    pub const TEXT_MESSAGE: u16 = 11;
    pub const PERMISSION_DENIED: u16 = 12;
    pub const CODEC_VERSION: u16 = 21;
    pub const SERVER_CONFIG: u16 = 24;
}

/// We announce 1.4.0 so the server keeps sending the legacy voice packet
/// format through the TCP tunnel.
pub const VERSION_V1: u32 = (1 << 16) | (4 << 8);
pub const VERSION_V2: u64 = (1 << 48) | (4 << 32);

pub const CODEC_OPUS: u8 = 4;

pub struct Frame {
    pub kind: u16,
    pub payload: Vec<u8>,
}

pub struct Connection {
    writer: WriteHalf<TlsStream<TcpStream>>,
    frames: mpsc::Receiver<std::io::Result<Frame>>,
}

#[derive(Debug)]
struct NoVerify(Arc<CryptoProvider>);

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

pub struct Identity {
    pub cert: CertificateDer<'static>,
    pub key: PrivateKeyDer<'static>,
}

impl Clone for Identity {
    fn clone(&self) -> Self {
        Self { cert: self.cert.clone(), key: self.key.clone_key() }
    }
}

/// Load the bot's client certificate, generating a long-lived self-signed one if missing.
/// A stable certificate lets server admins register the bot and grant it permissions.
pub fn load_or_create_identity(cert_path: &Path, key_path: &Path, common_name: &str) -> Result<Identity> {
    if !cert_path.exists() || !key_path.exists() {
        let mut params = rcgen::CertificateParams::new(vec![common_name.to_string()])?;
        params.distinguished_name.push(rcgen::DnType::CommonName, common_name);
        params.not_before = rcgen::date_time_ymd(2020, 1, 1);
        params.not_after = rcgen::date_time_ymd(2100, 1, 1);
        let key = rcgen::KeyPair::generate()?;
        let cert = params.self_signed(&key)?;
        if let Some(dir) = cert_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(cert_path, cert.pem())?;
        std::fs::write(key_path, key.serialize_pem())?;
        tracing::info!("generated bot certificate {}", cert_path.display());
    }
    let cert = CertificateDer::from_pem_file(cert_path).map_err(|e| anyhow!("read {}: {e}", cert_path.display()))?;
    let key = PrivateKeyDer::from_pem_file(key_path).map_err(|e| anyhow!("read {}: {e}", key_path.display()))?;
    Ok(Identity { cert, key })
}

impl Connection {
    pub async fn connect(address: &str, verify: bool, id: &Identity) -> Result<Self> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let builder = rustls::ClientConfig::builder_with_provider(provider.clone()).with_safe_default_protocol_versions()?;
        let builder = if verify {
            let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
            builder.with_root_certificates(roots)
        } else {
            builder.dangerous().with_custom_certificate_verifier(Arc::new(NoVerify(provider)))
        };
        let tls = builder.with_client_auth_cert(vec![id.cert.clone()], id.key.clone_key())?;

        let host = address.rsplit_once(':').map(|(h, _)| h).unwrap_or(address);
        let host = host.trim_start_matches('[').trim_end_matches(']').to_string();
        let addr = if address.contains(':') { address.to_string() } else { format!("{address}:64738") };
        let tcp = tokio::time::timeout(std::time::Duration::from_secs(15), TcpStream::connect(&addr))
            .await
            .context("connect timeout")?
            .with_context(|| format!("connect {addr}"))?;
        tcp.set_nodelay(true)?;
        let name = ServerName::try_from(host).map_err(|e| anyhow!("bad host: {e}"))?;
        let stream = tokio_rustls::TlsConnector::from(Arc::new(tls)).connect(name, tcp).await.context("TLS handshake")?;
        let (mut reader, writer) = tokio::io::split(stream);

        // Reading lives in its own task so `recv` is cancel-safe inside select!.
        let (tx, frames) = mpsc::channel(1024);
        tokio::spawn(async move {
            loop {
                let mut hdr = [0u8; 6];
                let r = async {
                    reader.read_exact(&mut hdr).await?;
                    let kind = u16::from_be_bytes([hdr[0], hdr[1]]);
                    let len = u32::from_be_bytes([hdr[2], hdr[3], hdr[4], hdr[5]]) as usize;
                    if len > 8 * 1024 * 1024 {
                        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "frame too large"));
                    }
                    let mut payload = vec![0u8; len];
                    reader.read_exact(&mut payload).await?;
                    Ok(Frame { kind, payload })
                }
                .await;
                let stop = r.is_err();
                if tx.send(r).await.is_err() || stop {
                    break;
                }
            }
        });
        Ok(Self { writer, frames })
    }

    pub async fn send_raw(&mut self, kind: u16, payload: &[u8]) -> Result<()> {
        let mut buf = Vec::with_capacity(6 + payload.len());
        buf.extend_from_slice(&kind.to_be_bytes());
        buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        buf.extend_from_slice(payload);
        self.writer.write_all(&buf).await?;
        Ok(())
    }

    pub async fn send<M: prost::Message>(&mut self, kind: u16, msg: &M) -> Result<()> {
        self.send_raw(kind, &msg.encode_to_vec()).await
    }

    /// Next frame from the server; Err on disconnect.
    pub async fn recv(&mut self) -> Result<Frame> {
        match self.frames.recv().await {
            Some(Ok(f)) => Ok(f),
            Some(Err(e)) => Err(e.into()),
            None => bail!("connection closed"),
        }
    }

    /// Discard any queued incoming frames (for send-only users like the simulator).
    pub fn drain(&mut self) {
        while self.frames.try_recv().is_ok() {}
    }

    /// Version + Authenticate. The caller then processes frames until ServerSync.
    pub async fn handshake(&mut self, username: &str, password: &str, tokens: &[String], bot: bool) -> Result<()> {
        self.send(
            kind::VERSION,
            &pb::Version {
                version_v1: Some(VERSION_V1),
                version_v2: Some(VERSION_V2),
                release: Some(format!("mumble-archive {}", env!("CARGO_PKG_VERSION"))),
                os: Some(std::env::consts::OS.into()),
                os_version: Some(String::new()),
            },
        )
        .await?;
        self.send(
            kind::AUTHENTICATE,
            &pb::Authenticate {
                username: Some(username.into()),
                password: (!password.is_empty()).then(|| password.into()),
                tokens: tokens.to_vec(),
                celt_versions: vec![],
                opus: Some(true),
                client_type: Some(if bot { 1 } else { 0 }),
            },
        )
        .await
    }
}

#[derive(Debug, Clone)]
pub struct AudioPacket {
    pub codec: u8,
    pub target: u8,
    pub session: u32,
    pub sequence: i64,
    pub opus: Vec<u8>,
    pub terminator: bool,
}

/// Parse a server→client legacy voice packet (as carried in UDPTunnel).
pub fn parse_server_audio(buf: &[u8]) -> Option<AudioPacket> {
    let header = *buf.first()?;
    let codec = header >> 5;
    let target = header & 0x1F;
    if codec == 1 {
        return None; // UDP ping
    }
    let mut r = varint::Reader::new(&buf[1..]);
    let session = r.varint()? as u32;
    let sequence = r.varint()?;
    if codec != CODEC_OPUS {
        return Some(AudioPacket { codec, target, session, sequence, opus: vec![], terminator: false });
    }
    let len = r.varint()?;
    let terminator = len & 0x2000 != 0;
    let len = (len & 0x1FFF) as usize;
    let opus = r.bytes(len)?.to_vec();
    Some(AudioPacket { codec, target, session, sequence, opus, terminator })
}

/// Build a client→server legacy Opus voice packet (no session field).
pub fn build_client_audio(target: u8, sequence: u64, opus: &[u8], terminator: bool) -> Vec<u8> {
    let mut out = vec![(CODEC_OPUS << 5) | (target & 0x1F)];
    varint::write(&mut out, sequence);
    varint::write(&mut out, opus.len() as u64 | if terminator { 0x2000 } else { 0 });
    out.extend_from_slice(opus);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_roundtrip() {
        // server packet = client packet with session inserted after header
        let client = build_client_audio(0, 1234, &[1, 2, 3], true);
        let mut server = vec![client[0]];
        varint::write(&mut server, 42);
        server.extend_from_slice(&client[1..]);
        let p = parse_server_audio(&server).unwrap();
        assert_eq!((p.session, p.sequence, p.terminator, p.opus.as_slice()), (42, 1234, true, &[1u8, 2, 3][..]));
    }
}
