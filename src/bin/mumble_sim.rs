//! Test helper: a fake Mumble speaker. Connects, optionally creates/joins a
//! channel and streams an Ogg Opus file through the TCP voice tunnel in real time.
//!
//!   mumble-sim --server 127.0.0.1:64738 --name Alice --channel Lobby --file speech.opus --repeat 3 --pause 2000

use anyhow::{bail, Context, Result};
use mumble_archive::mumble::{self, kind, pb, Connection};
use mumble_archive::{ogg, opus};
use prost::Message;
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[tokio::main]
async fn main() -> Result<()> {
    let mut opts: HashMap<String, String> = HashMap::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let k = a.trim_start_matches("--").to_string();
        opts.insert(k, args.next().unwrap_or_default());
    }
    let get = |k: &str, d: &str| opts.get(k).cloned().unwrap_or_else(|| d.to_string());
    let server = get("server", "127.0.0.1:64738");
    let name = get("name", "SimSpeaker");
    let channel = get("channel", "");
    let tree: usize = get("tree", "0").parse()?;
    let file = if tree > 0 { String::new() } else { opts.get("file").context("--file <ogg opus> required")?.clone() };
    let repeat: u32 = get("repeat", "1").parse()?;
    let pause: u64 = get("pause", "1500").parse()?;
    let start_delay: u64 = get("delay", "0").parse()?;
    let create = get("create", "") == "1";
    let drop_every: usize = get("drop", "0").parse()?;

    let packets = if tree > 0 { vec![] } else { ogg::read_opus_packets(&std::fs::read(&file)?) };
    if packets.is_empty() && tree == 0 {
        bail!("no packets in {file}");
    }
    let dir = std::env::temp_dir().join(format!("mumble-sim-{name}"));
    let id = mumble::load_or_create_identity(&dir.join("cert.pem"), &dir.join("key.pem"), &name)?;
    let mut conn = Connection::connect(&server, false, &id).await?;
    conn.handshake(&name, &get("password", ""), &[], false).await?;

    let mut channels: HashMap<String, u32> = HashMap::new();
    let session;
    loop {
        let f = conn.recv().await?;
        match f.kind {
            kind::CHANNEL_STATE => {
                let m = pb::ChannelState::decode(&f.payload[..])?;
                if let (Some(id), Some(n)) = (m.channel_id, m.name) {
                    channels.insert(n, id);
                }
            }
            kind::SERVER_SYNC => {
                session = pb::ServerSync::decode(&f.payload[..])?.session.unwrap_or(0);
                break;
            }
            kind::REJECT => bail!("rejected: {:?}", pb::Reject::decode(&f.payload[..])?.reason),
            _ => {}
        }
    }
    println!("{name}: connected as session {session}");

    if tree > 0 {
        let depth: usize = get("depth", "30").parse()?;
        return build_tree(&mut conn, &mut channels, tree, depth).await;
    }

    if !channel.is_empty() {
        if !channels.contains_key(&channel) && create {
            conn.send(
                kind::CHANNEL_STATE,
                &pb::ChannelState { parent: Some(0), name: Some(channel.clone()), temporary: Some(false), ..Default::default() },
            )
            .await?;
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                let f = tokio::time::timeout(Duration::from_secs(5), conn.recv()).await??;
                if f.kind == kind::CHANNEL_STATE {
                    let m = pb::ChannelState::decode(&f.payload[..])?;
                    if let (Some(id), Some(n)) = (m.channel_id, m.name) {
                        channels.insert(n, id);
                        if channels.contains_key(&channel) {
                            break;
                        }
                    }
                } else if f.kind == kind::PERMISSION_DENIED {
                    bail!("cannot create channel: {:?}", pb::PermissionDenied::decode(&f.payload[..])?);
                }
            }
        }
        let cid = *channels.get(&channel).with_context(|| format!("channel {channel} not found"))?;
        conn.send(kind::USER_STATE, &pb::UserState { session: Some(session), channel_id: Some(cid), ..Default::default() })
            .await?;
        println!("{name}: joined {channel} ({cid})");
    }

    tokio::time::sleep(Duration::from_millis(500 + start_delay)).await;

    let mut seq: u64 = 0;
    for r in 0..repeat {
        let t0 = Instant::now();
        let mut elapsed_samples: i64 = 0;
        for (i, p) in packets.iter().enumerate() {
            let n = opus::packet_samples(p).unwrap_or(960);
            let last = i == packets.len() - 1;
            let pkt = mumble::build_client_audio(0, seq, p, last);
            // Simulate packet loss between client and server.
            if last || drop_every == 0 || i % drop_every != drop_every - 1 {
                conn.send_raw(kind::UDP_TUNNEL, &pkt).await?;
            }
            seq += (n / 480) as u64;
            conn.drain();
            elapsed_samples += n;
            let due = t0 + Duration::from_micros((elapsed_samples * 1_000_000 / 48_000) as u64);
            tokio::time::sleep_until(due.into()).await;
        }
        println!("{name}: burst {} done ({:.1}s)", r + 1, elapsed_samples as f64 / 48_000.0);
        // Mumble clients keep counting sequence across silence.
        seq += pause / 10;
        tokio::time::sleep(Duration::from_millis(pause)).await;
        conn.send(kind::PING, &pb::Ping { timestamp: Some(0), ..Default::default() }).await?;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    Ok(())
}

/// Create a test channel tree under Root: a spine "deep1/deep2/…/deepN" plus
/// pseudo-random branches ("c<n>") until `total` channels exist. Created level
/// by level, since children need their parent's id.
async fn build_tree(conn: &mut Connection, channels: &mut HashMap<String, u32>, total: usize, depth: usize) -> Result<()> {
    // plan: (name, parent index or None for Root)
    let mut plan: Vec<(String, Option<usize>, usize)> = vec![]; // name, parent, level
    for d in 0..depth.min(total) {
        plan.push((format!("deep{}", d + 1), d.checked_sub(1), d + 1));
    }
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
    while plan.len() < total {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        // prefer shallow parents so the tree is wide as well as deep
        let pick = (seed % (plan.len() as u64 + 1)) as usize;
        let parent = if pick == plan.len() || plan[pick].2 >= depth { None } else { Some(pick) };
        let level = parent.map(|p| plan[p].2 + 1).unwrap_or(1);
        plan.push((format!("c{}", plan.len()), parent, level));
    }
    let max_level = plan.iter().map(|p| p.2).max().unwrap_or(1);
    let mut ids: Vec<Option<u32>> = plan.iter().map(|p| channels.get(&p.0).copied()).collect();
    let t0 = Instant::now();
    for level in 1..=max_level {
        // Servers may drop bursts of requests; resend whatever is still missing.
        for attempt in 0..20 {
            let mut pending = 0usize;
            for (i, (name, parent, l)) in plan.iter().enumerate() {
                if *l != level || ids[i].is_some() {
                    continue;
                }
                let parent_id = match parent {
                    None => 0,
                    Some(p) => ids[*p].context("parent missing")?,
                };
                conn.send(
                    kind::CHANNEL_STATE,
                    &pb::ChannelState { parent: Some(parent_id), name: Some(name.clone()), temporary: Some(false), ..Default::default() },
                )
                .await?;
                pending += 1;
            }
            if pending == 0 {
                break;
            }
            if attempt > 0 {
                eprintln!("(level {level}: resending {pending})");
            }
            while pending > 0 {
                let Ok(f) = tokio::time::timeout(Duration::from_secs(3), conn.recv()).await else { break };
                let f = f?;
                match f.kind {
                    kind::CHANNEL_STATE => {
                        let m = pb::ChannelState::decode(&f.payload[..])?;
                        if let (Some(id), Some(n)) = (m.channel_id, m.name) {
                            if let Some(i) = plan.iter().position(|p| p.0 == n) {
                                if ids[i].is_none() {
                                    ids[i] = Some(id);
                                    pending -= 1;
                                }
                            }
                            channels.insert(n, id);
                        }
                    }
                    kind::PERMISSION_DENIED => {
                        let d = pb::PermissionDenied::decode(&f.payload[..])?;
                        if d.r#type == Some(pb::permission_denied::DenyType::ChannelName as i32) {
                            println!("already exists on server: resync needed (run again)");
                            return Ok(());
                        }
                        bail!("server refused: {d:?}");
                    }
                    _ => {}
                }
            }
        }
        println!("level {level}: {} channels so far ({:.1}s)", ids.iter().filter(|x| x.is_some()).count(), t0.elapsed().as_secs_f64());
    }
    println!("tree complete: {} channels", ids.iter().filter(|x| x.is_some()).count());
    Ok(())
}
