//! HELIOS CLI entry point.
//!
//! Subcommands:
//!   helios serve <app.js>             — run the dual-stack HTTP/1+2+3 server.
//!   helios build <app.js> -o app.wasm — Phase 5 Wizer build.
//!   helios bench <url>                — built-in load generator.
//!   helios exec  <script.js>          — one-shot script execution.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use tracing_subscriber::EnvFilter;

use helios::bench;
use helios::boa_backend::BoaEngine;
use helios::dispatcher::{DispatchPolicy, HeliosDispatcher};
use helios::engine::JsEngineBackend;
use helios::http3::{run_server, InlineEngine, ServerConfig, TlsConfig};
use helios::wizer_build;
use helios::xdr::{UserCode, XdrCache};

#[derive(Parser, Debug)]
#[command(name = "helios", version, about = "HELIOS — JIT at the edge. Finally.")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Start the HELIOS server (HTTP/1+2 + HTTP/3 + WebTransport).
    Serve(Serve),
    /// Build a pre-initialized `.wasm` via Wizer (Phase 5).
    Build(Build),
    /// Built-in load generator (`helios bench <url>`).
    Bench(Bench),
    /// Execute a JS file once and exit.
    Exec(Exec),
}

#[derive(Parser, Debug)]
struct Serve {
    /// JS entry point (file or directory).
    js_path: PathBuf,
    /// Run in script mode (no module resolution).
    #[arg(short, long)]
    script: bool,
    /// Bind interface.
    #[arg(long, default_value_t = IpAddr::V4(Ipv4Addr::UNSPECIFIED))]
    ip: IpAddr,
    /// Bind port.
    #[arg(short, long, default_value_t = 8080)]
    port: u16,
    /// Number of worker threads.
    #[arg(long, default_value_t = num_cpus())]
    workers: usize,
    /// Dispatch policy (Phase 1).
    #[arg(long, value_enum, default_value_t = Policy::PowerOfTwo)]
    policy: Policy,
    /// TLS cert chain (PEM). Enables HTTP/3 when paired with --key.
    #[arg(long)]
    cert: Option<PathBuf>,
    /// TLS private key (PEM).
    #[arg(long)]
    key: Option<PathBuf>,
    /// Alt-Svc header value advertised on HTTP/1+2 (e.g. `h3=":8443"; ma=86400`).
    #[arg(long)]
    alt_svc: Option<String>,
    /// Graceful shutdown timeout in seconds. 0 = disabled.
    #[arg(short = 't', long, default_value_t = 60)]
    shutdown_timeout: u64,
}

#[derive(Parser, Debug)]
struct Build {
    /// JS entry point.
    js_path: PathBuf,
    /// Path to the pre-built helios-worker.wasm component.
    #[arg(long, default_value = "helios-worker.wasm")]
    worker_wasm: PathBuf,
    /// Output .wasm path.
    #[arg(short, long, default_value = "app.wasm")]
    output: PathBuf,
    /// Target.
    #[arg(long, value_enum, default_value_t = BuildTargetArg::Wasip2)]
    target: BuildTargetArg,
    /// Skip wasm-opt post-processing.
    #[arg(long)]
    no_opt: bool,
}

#[derive(Parser, Debug)]
struct Bench {
    /// Target URL.
    url: String,
    /// Test duration in seconds.
    #[arg(short, long, default_value_t = 30)]
    duration: u64,
    /// Warmup duration in seconds (excluded from results).
    #[arg(long, default_value_t = 3)]
    warmup: u64,
    /// Concurrent connections.
    #[arg(short, long, default_value_t = 64)]
    connections: usize,
    /// Target rate, requests/sec across all connections (for tail-latency
    /// measurement with coordinated-omission correction).
    #[arg(short = 'R', long)]
    rate: Option<u64>,
    /// HTTP version.
    #[arg(long, value_enum, default_value_t = HttpVerArg::Auto)]
    http: HttpVerArg,
    /// Output JSON instead of human-readable.
    #[arg(long)]
    json: bool,
    /// Optional POST body file.
    #[arg(long)]
    body_file: Option<PathBuf>,
}

#[derive(Parser, Debug)]
struct Exec {
    js_path: PathBuf,
    #[arg(short, long)]
    script: bool,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum Policy {
    RoundRobin,
    LeastLoaded,
    PowerOfTwo,
}

impl From<Policy> for DispatchPolicy {
    fn from(p: Policy) -> Self {
        match p {
            Policy::RoundRobin => DispatchPolicy::RoundRobin,
            Policy::LeastLoaded => DispatchPolicy::LeastLoaded,
            Policy::PowerOfTwo => DispatchPolicy::PowerOfTwo,
        }
    }
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum BuildTargetArg {
    Wasip2,
}
impl From<BuildTargetArg> for wizer_build::BuildTarget {
    fn from(_: BuildTargetArg) -> Self {
        wizer_build::BuildTarget::Wasip2
    }
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum HttpVerArg {
    Auto,
    Http1,
    Http2,
    Http3,
}
impl From<HttpVerArg> for bench::HttpVersion {
    fn from(v: HttpVerArg) -> Self {
        match v {
            HttpVerArg::Auto => bench::HttpVersion::Auto,
            HttpVerArg::Http1 => bench::HttpVersion::Http1,
            HttpVerArg::Http2 => bench::HttpVersion::Http2,
            HttpVerArg::Http3 => bench::HttpVersion::Http3,
        }
    }
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8)
}

fn install_tracing() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("helios=info,warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .try_init();
}

fn main() -> Result<()> {
    install_tracing();
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        match cli.cmd {
            Cmd::Serve(s) => cmd_serve(s).await,
            Cmd::Build(b) => cmd_build(b),
            Cmd::Bench(b) => cmd_bench(b).await,
            Cmd::Exec(e) => cmd_exec(e),
        }
    })
}

async fn cmd_serve(s: Serve) -> Result<()> {
    let user_code = UserCode::from_path(&s.js_path, s.script)?;
    let cache = Arc::new(XdrCache::new());
    let bootstrap_engine = Arc::new(BoaEngine::new());
    cache.compile_user_code(bootstrap_engine.as_ref(), &user_code)?;

    let entry = cache
        .first_active()
        .ok_or_else(|| anyhow::anyhow!("compiled module missing from XDR cache"))?;
    let inline_handle = bootstrap_engine
        .eval_xdr(entry.bytecode.clone(), &entry.module_url)
        .map_err(|e| anyhow::anyhow!("inline engine warmup failed: {e}"))?;

    let dispatcher =
        HeliosDispatcher::spawn(s.workers, s.policy.into(), cache, || Ok(BoaEngine::new()))?;
    tracing::info!(workers = dispatcher.worker_count(),
        policy = ?DispatchPolicy::from(s.policy), "dispatcher up");

    let addr = SocketAddr::new(s.ip, s.port);
    let mut cfg = ServerConfig::plain(addr).with_inline_engine(InlineEngine {
        engine: bootstrap_engine,
        handle: inline_handle,
    });
    if let Some(av) = s.alt_svc {
        cfg = cfg.with_alt_svc(av);
    }
    match (s.cert, s.key) {
        (Some(cert), Some(key)) => {
            cfg = cfg.with_tls(TlsConfig {
                cert_chain_pem_path: cert,
                private_key_pem_path: key,
            });
        }
        (Some(_), None) | (None, Some(_)) => {
            anyhow::bail!("--cert and --key must be provided together")
        }
        _ => {}
    }

    let (tx, rx) = tokio::sync::oneshot::channel();
    let timeout = if s.shutdown_timeout == 0 {
        None
    } else {
        Some(Duration::from_secs(s.shutdown_timeout))
    };
    let dispatcher_for_signal = dispatcher.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("ctrl-c received; shutting down");
        dispatcher_for_signal.shutdown(timeout).await;
        let _ = tx.send(());
    });

    run_server(cfg, dispatcher, rx).await
}

fn cmd_build(b: Build) -> Result<()> {
    let cfg = wizer_build::BuildConfig {
        js_path: b.js_path,
        worker_wasm: b.worker_wasm,
        output: b.output,
        target: b.target.into(),
        init_func: "helios_init".to_string(),
        wasm_opt: !b.no_opt,
    };
    let outcome = wizer_build::build(&cfg)?;
    println!(
        "✓ wrote {} ({} bytes)",
        outcome.output.display(),
        outcome.bytes
    );
    Ok(())
}

async fn cmd_bench(b: Bench) -> Result<()> {
    let body = match b.body_file {
        Some(p) => Some(std::fs::read(&p)?),
        None => None,
    };
    let cfg = bench::BenchConfig {
        url: b.url,
        duration: Duration::from_secs(b.duration),
        warmup: Duration::from_secs(b.warmup),
        connections: b.connections,
        rate_per_sec: b.rate,
        http_version: b.http.into(),
        body,
    };
    let report = bench::run(&cfg).await?;
    if b.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("{}", bench::render_human(&report));
    }
    Ok(())
}

fn cmd_exec(e: Exec) -> Result<()> {
    let source = std::fs::read_to_string(&e.js_path)
        .map_err(|err| anyhow::anyhow!("failed to read {}: {err}", e.js_path.display()))?;
    let module_url = e.js_path.display().to_string();
    let engine = BoaEngine::new();
    engine
        .eval_module(&source, &module_url)
        .map_err(|err| anyhow::anyhow!("exec failed: {err}"))?;
    Ok(())
}
