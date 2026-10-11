use mumble_archive::{bot, config, export, live, mumble, store, transcode};

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

const USAGE: &str = "usage:
  mumble-archive [--config config.toml]                 run the recording bot
  mumble-archive republish [--config config.toml]       rebuild all static index files
  mumble-archive export --from <time> --to <time> --out <file>
        [--layout downmix|tracks|channels|stems] [--format wav|flac|opus]
        [--channels 1,2] [--speakers 3,4] [--config config.toml]
      <time> is unix ms or local 'YYYY-MM-DD HH:MM[:SS]'";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = if args.first().is_some_and(|a| !a.starts_with('-')) { args.remove(0) } else { "run".into() };
    let mut opts: HashMap<String, String> = HashMap::new();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            "-c" => {
                opts.insert("config".into(), it.next().context("-c needs a path")?);
            }
            a if a.starts_with("--") => {
                opts.insert(a[2..].to_string(), it.next().with_context(|| format!("{a} needs a value"))?);
            }
            other => bail!("unknown argument {other}\n{USAGE}"),
        }
    }
    let config_path = PathBuf::from(opts.get("config").cloned().unwrap_or_else(|| "config.toml".into()));
    let cfg = Arc::new(config::Config::load(&config_path)?);
    std::fs::create_dir_all(&cfg.recording.data_dir)?;
    std::fs::create_dir_all(cfg.public_dir())?;


    let store = Arc::new(store::Store::open(&cfg.recording.data_dir.join("archive.db"))?);
    let live = Arc::new(live::Live::new());
    let publisher = Arc::new(live::Publisher { cfg: cfg.clone(), store: store.clone(), live: live.clone() });

    match cmd.as_str() {
        "run" => run(cfg, store, live, publisher).await,
        "republish" => {
            publisher.rebuild(true)?;
            println!("rebuilt static indexes in {}", cfg.public_dir().display());
            Ok(())
        }
        "export" => export_cli(cfg, store, &opts).await,
        other => bail!("unknown command {other}\n{USAGE}"),
    }
}

async fn run(cfg: Arc<config::Config>, store: Arc<store::Store>, live: Arc<live::Live>, publisher: Arc<live::Publisher>) -> Result<()> {
    store.close_dangling()?;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let transcoder = Arc::new(transcode::Transcoder {
        cfg: cfg.clone(),
        store: store.clone(),
        live: live.clone(),
        pack_lock: Default::default(),
    });
    transcoder.recover(&tx)?;
    mumble_archive::recorder::recover_orphans(&cfg, &store, &tx)?;
    tokio::spawn(transcoder.run(rx));
    tokio::spawn(transcode::retention(cfg.clone(), store.clone(), live.clone()));
    publisher.rebuild(false)?;
    tokio::spawn(publisher.run());

    let identity = mumble::load_or_create_identity(
        &cfg.data_path(&cfg.mumble.cert_file),
        &cfg.data_path(&cfg.mumble.key_file),
        &cfg.mumble.username,
    )?;
    let recorder = mumble_archive::recorder::Recorder::spawn(cfg.clone(), store.clone(), live.clone(), tx);
    let bot = Arc::new(bot::Bot { cfg: cfg.clone(), store, live, recorder, identity });
    tracing::info!("publishing static files to {}", cfg.public_dir().display());
    tokio::select! {
        _ = bot.run() => {}
        _ = tokio::signal::ctrl_c() => tracing::info!("shutting down"),
    }
    Ok(())
}

fn parse_time(s: &str) -> Result<i64> {
    use chrono::TimeZone;
    if let Ok(ms) = s.parse::<i64>() {
        return Ok(ms);
    }
    for f in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, f) {
            if let Some(t) = chrono::Local.from_local_datetime(&t).earliest() {
                return Ok(t.timestamp_millis());
            }
        }
    }
    bail!("cannot parse time '{s}'")
}

async fn export_cli(cfg: Arc<config::Config>, store: Arc<store::Store>, o: &HashMap<String, String>) -> Result<()> {
    let list = |k: &str| -> Vec<i64> {
        o.get(k).map(|s| s.split(',').filter_map(|x| x.trim().parse().ok()).collect()).unwrap_or_default()
    };
    let req = export::Request {
        from: parse_time(o.get("from").context("--from required")?)?,
        to: parse_time(o.get("to").context("--to required")?)?,
        filter: store::Filter { channels: list("channels"), speakers: list("speakers"), tracks: vec![] },
        gains: Default::default(),
        layout: export::Layout::parse(o.get("layout").map(|s| s.as_str()).unwrap_or("downmix"))?,
        format: export::Format::parse(o.get("format").map(|s| s.as_str()).unwrap_or("flac"))?,
    };
    let plan = export::plan(&store, &req)?;
    for c in &plan.channels {
        println!("channel {}: {}", c.index + 1, c.label);
    }
    for w in &plan.warnings {
        eprintln!("warning: {w}");
    }
    let out_path = o.get("out").cloned().unwrap_or_else(|| plan.filename.clone());
    let mut out = export::run(cfg, plan, req.format, req.layout == export::Layout::Stems);
    let mut f = tokio::fs::File::create(&out_path).await?;
    while let Some(chunk) = out.body.recv().await {
        f.write_all(&chunk?).await?;
    }
    f.flush().await?;
    println!("wrote {out_path}");
    Ok(())
}
