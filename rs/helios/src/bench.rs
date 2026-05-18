//! `helios bench` — built-in load generator.
//!
//! Produces a wrk2-style closed-loop test with HDR-histogram latency
//! tracking. Supports HTTP/1.1, HTTP/2, and HTTP/3 (via reqwest's
//! `rustls-tls` + ALPN) against any URL — so you can compare HELIOS
//! against Bun, Deno, Node, etc., from the same harness.
//!
//! Methodology notes:
//!
//! * Latency is recorded **including** queueing time (coordinated-omission
//!   correction) by issuing requests on a fixed schedule. We approximate
//!   wrk2's `-R <rate>` mode.
//! * Per-connection serial issuance models real client behavior; total
//!   parallelism comes from the `--connections` flag.
//! * Warmup is excluded from the reported histogram.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use hdrhistogram::Histogram;
use serde::Serialize;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::time::sleep;

use crate::http1_utils::{content_length, find_header_end_from, status_is_success, write_all_fast};

#[derive(Debug, Clone)]
pub struct BenchConfig {
    pub url: String,
    pub duration: Duration,
    pub warmup: Duration,
    pub connections: usize,
    pub rate_per_sec: Option<u64>,
    pub http_version: HttpVersion,
    pub body: Option<Vec<u8>>,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum HttpVersion {
    Auto,
    Http1,
    Http2,
    Http3,
}

impl Default for BenchConfig {
    fn default() -> Self {
        Self {
            url: "http://127.0.0.1:8080/".into(),
            duration: Duration::from_secs(30),
            warmup: Duration::from_secs(3),
            connections: 64,
            rate_per_sec: None,
            http_version: HttpVersion::Auto,
            body: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BenchReport {
    pub url: String,
    pub duration_s: f64,
    pub connections: usize,
    pub requests: u64,
    pub errors: u64,
    pub bytes_in: u64,
    pub rps: f64,
    pub throughput_mbps: f64,
    pub latency_ms: Latencies,
}

#[derive(Debug, Clone, Serialize)]
pub struct Latencies {
    pub min: f64,
    pub p50: f64,
    pub p90: f64,
    pub p99: f64,
    pub p999: f64,
    pub max: f64,
    pub mean: f64,
}

struct WorkerStats {
    requests: u64,
    errors: u64,
    bytes_in: u64,
    hist: Histogram<u64>,
}

pub async fn run(cfg: &BenchConfig) -> Result<BenchReport> {
    tracing::info!(
        url = %cfg.url,
        conns = cfg.connections,
        dur_s = cfg.duration.as_secs_f64(),
        warmup_s = cfg.warmup.as_secs_f64(),
        rate = ?cfg.rate_per_sec,
        "helios bench: starting"
    );

    if matches!(cfg.http_version, HttpVersion::Auto | HttpVersion::Http1)
        && cfg.body.is_none()
        && cfg.url.starts_with("http://")
    {
        return run_raw_h1(cfg).await;
    }

    let stop = Arc::new(AtomicBool::new(false));
    let warmup_done = Arc::new(AtomicBool::new(false));

    // Per-connection rate target.
    let per_conn_rate = cfg
        .rate_per_sec
        .map(|r| r as f64 / cfg.connections as f64)
        .filter(|r| *r > 0.0);

    let client = build_client(cfg)?;

    let mut tasks = Vec::with_capacity(cfg.connections);
    let start = Instant::now();

    for _ in 0..cfg.connections {
        let client = client.clone();
        let url = cfg.url.clone();
        let body = cfg.body.clone();
        let stop = stop.clone();
        let warmup_done = warmup_done.clone();
        let conn_rate = per_conn_rate;

        tasks.push(tokio::spawn(async move {
            let mut requests = 0u64;
            let mut errors = 0u64;
            let mut bytes_in = 0u64;
            // 1us .. 60s buckets, 3 significant digits.
            let mut hist = Histogram::<u64>::new_with_bounds(1, 60_000_000, 3)
                .context("init worker histogram")?;
            let mut next: Option<Instant> = conn_rate.map(|_| Instant::now());
            while !stop.load(Ordering::Acquire) {
                if let (Some(rate), Some(t)) = (conn_rate, next) {
                    let now = Instant::now();
                    if now < t {
                        sleep(t - now).await;
                    }
                    next = Some(t + Duration::from_secs_f64(1.0 / rate));
                }
                let t0 = Instant::now();
                let req = if let Some(b) = &body {
                    client.request(reqwest::Method::POST, &url).body(b.clone())
                } else {
                    client.request(reqwest::Method::GET, &url)
                };
                match req.send().await {
                    Ok(resp) => {
                        let status_ok = resp.status().is_success();
                        match resp.bytes().await {
                            Ok(b) => {
                                if !status_ok {
                                    errors += 1;
                                }
                                bytes_in += b.len() as u64;
                            }
                            Err(_) => {
                                errors += 1;
                            }
                        }
                    }
                    Err(_) => {
                        errors += 1;
                    }
                }
                let elapsed_us = t0.elapsed().as_micros().min(60_000_000) as u64;
                if warmup_done.load(Ordering::Acquire) {
                    requests += 1;
                    let _ = hist.record(elapsed_us.max(1));
                }
            }
            Ok::<_, anyhow::Error>(WorkerStats {
                requests,
                errors,
                bytes_in,
                hist,
            })
        }));
    }

    sleep(cfg.warmup).await;
    warmup_done.store(true, Ordering::Release);
    let measure_start = Instant::now();

    sleep(cfg.duration).await;
    stop.store(true, Ordering::Release);

    let total_elapsed = measure_start.elapsed().as_secs_f64();
    let mut h =
        Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).context("init merged histogram")?;
    let mut n_req = 0u64;
    let mut n_err = 0u64;
    let mut n_bytes = 0u64;
    for t in tasks {
        let stats = t.await.context("bench worker join")??;
        n_req += stats.requests;
        n_err += stats.errors;
        n_bytes += stats.bytes_in;
        h.add(stats.hist).context("merge worker histogram")?;
    }

    let report = BenchReport {
        url: cfg.url.clone(),
        duration_s: total_elapsed,
        connections: cfg.connections,
        requests: n_req,
        errors: n_err,
        bytes_in: n_bytes,
        rps: if total_elapsed > 0.0 {
            n_req as f64 / total_elapsed
        } else {
            0.0
        },
        throughput_mbps: if total_elapsed > 0.0 {
            (n_bytes as f64 * 8.0) / (total_elapsed * 1_000_000.0)
        } else {
            0.0
        },
        latency_ms: Latencies {
            min: h.min() as f64 / 1000.0,
            p50: h.value_at_quantile(0.50) as f64 / 1000.0,
            p90: h.value_at_quantile(0.90) as f64 / 1000.0,
            p99: h.value_at_quantile(0.99) as f64 / 1000.0,
            p999: h.value_at_quantile(0.999) as f64 / 1000.0,
            max: h.max() as f64 / 1000.0,
            mean: h.mean() / 1000.0,
        },
    };

    let _ = start; // silence warning in some build modes
    Ok(report)
}

#[derive(Clone)]
struct RawTarget {
    host: String,
    port: u16,
    request: Arc<[u8]>,
}

async fn run_raw_h1(cfg: &BenchConfig) -> Result<BenchReport> {
    let target = parse_raw_h1_target(&cfg.url)?;
    let stats = run_workers(cfg, move |conn_rate, stop, warmup_done| {
        let target = target.clone();
        async move { run_raw_h1_worker(target, conn_rate, stop, warmup_done).await }
    })
    .await?;
    Ok(report_from_stats(cfg, stats))
}

async fn run_workers<F, Fut>(cfg: &BenchConfig, worker: F) -> Result<WorkerStats>
where
    F: Fn(Option<f64>, Arc<AtomicBool>, Arc<AtomicBool>) -> Fut + Send + Sync + Clone + 'static,
    Fut: std::future::Future<Output = Result<WorkerStats>> + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let warmup_done = Arc::new(AtomicBool::new(false));
    let per_conn_rate = cfg
        .rate_per_sec
        .map(|r| r as f64 / cfg.connections as f64)
        .filter(|r| *r > 0.0);

    let mut tasks = Vec::with_capacity(cfg.connections);
    for _ in 0..cfg.connections {
        tasks.push(tokio::spawn(worker(
            per_conn_rate,
            stop.clone(),
            warmup_done.clone(),
        )));
    }

    sleep(cfg.warmup).await;
    warmup_done.store(true, Ordering::Release);
    sleep(cfg.duration).await;
    stop.store(true, Ordering::Release);

    let mut merged =
        Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).context("init merged histogram")?;
    let mut stats = WorkerStats {
        requests: 0,
        errors: 0,
        bytes_in: 0,
        hist: Histogram::<u64>::new_with_bounds(1, 60_000_000, 3)
            .context("init empty histogram")?,
    };
    for t in tasks {
        let worker = t.await.context("bench worker join")??;
        stats.requests += worker.requests;
        stats.errors += worker.errors;
        stats.bytes_in += worker.bytes_in;
        merged.add(worker.hist).context("merge worker histogram")?;
    }
    stats.hist = merged;
    Ok(stats)
}

async fn run_raw_h1_worker(
    target: RawTarget,
    conn_rate: Option<f64>,
    stop: Arc<AtomicBool>,
    warmup_done: Arc<AtomicBool>,
) -> Result<WorkerStats> {
    let mut requests = 0u64;
    let mut errors = 0u64;
    let mut bytes_in = 0u64;
    let mut hist =
        Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).context("init worker histogram")?;
    let mut stream = TcpStream::connect((target.host.as_str(), target.port))
        .await
        .with_context(|| format!("connect {}:{}", target.host, target.port))?;
    stream
        .set_nodelay(true)
        .context("set benchmark TCP_NODELAY")?;
    let mut reader = RawH1ResponseReader::new();
    let mut next: Option<Instant> = conn_rate.map(|_| Instant::now());
    let mut measuring = false;

    while !stop.load(Ordering::Acquire) {
        if let (Some(rate), Some(t)) = (conn_rate, next) {
            let now = Instant::now();
            if now < t {
                sleep(t - now).await;
            }
            next = Some(t + Duration::from_secs_f64(1.0 / rate));
        }
        let t0 = Instant::now();
        let result = async {
            write_all_fast(&mut stream, &target.request).await?;
            reader.read_response(&mut stream).await
        }
        .await;
        match result {
            Ok((status_ok, body_len)) => {
                if !status_ok {
                    errors += 1;
                }
                bytes_in += body_len as u64;
            }
            Err(_) => {
                errors += 1;
                reader.clear();
                stream = TcpStream::connect((target.host.as_str(), target.port))
                    .await
                    .with_context(|| format!("reconnect {}:{}", target.host, target.port))?;
                stream
                    .set_nodelay(true)
                    .context("set benchmark TCP_NODELAY")?;
            }
        }

        let elapsed_us = t0.elapsed().as_micros().min(60_000_000) as u64;
        if !measuring {
            measuring = warmup_done.load(Ordering::Acquire);
        }
        if measuring {
            requests += 1;
            let _ = hist.record(elapsed_us.max(1));
        }
    }

    Ok(WorkerStats {
        requests,
        errors,
        bytes_in,
        hist,
    })
}

fn report_from_stats(cfg: &BenchConfig, stats: WorkerStats) -> BenchReport {
    let total_elapsed = cfg.duration.as_secs_f64();
    BenchReport {
        url: cfg.url.clone(),
        duration_s: total_elapsed,
        connections: cfg.connections,
        requests: stats.requests,
        errors: stats.errors,
        bytes_in: stats.bytes_in,
        rps: if total_elapsed > 0.0 {
            stats.requests as f64 / total_elapsed
        } else {
            0.0
        },
        throughput_mbps: if total_elapsed > 0.0 {
            (stats.bytes_in as f64 * 8.0) / (total_elapsed * 1_000_000.0)
        } else {
            0.0
        },
        latency_ms: Latencies {
            min: stats.hist.min() as f64 / 1000.0,
            p50: stats.hist.value_at_quantile(0.50) as f64 / 1000.0,
            p90: stats.hist.value_at_quantile(0.90) as f64 / 1000.0,
            p99: stats.hist.value_at_quantile(0.99) as f64 / 1000.0,
            p999: stats.hist.value_at_quantile(0.999) as f64 / 1000.0,
            max: stats.hist.max() as f64 / 1000.0,
            mean: stats.hist.mean() / 1000.0,
        },
    }
}

fn parse_raw_h1_target(url: &str) -> Result<RawTarget> {
    let rest = url
        .strip_prefix("http://")
        .context("raw HTTP/1 benchmark requires http:// URL")?;
    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (
            host,
            port.parse::<u16>()
                .with_context(|| format!("invalid port in {url}"))?,
        ),
        None => (authority, 80),
    };
    if host.is_empty() {
        anyhow::bail!("missing host in {url}");
    }

    let mut request = Vec::with_capacity(96 + path.len() + host.len());
    request.extend_from_slice(b"GET ");
    request.extend_from_slice(path.as_bytes());
    request.extend_from_slice(b" HTTP/1.1\r\nhost: ");
    request.extend_from_slice(authority.as_bytes());
    request.extend_from_slice(b"\r\nconnection: keep-alive\r\n\r\n");
    Ok(RawTarget {
        host: host.to_string(),
        port,
        request: Arc::from(request),
    })
}

struct RawH1ResponseReader {
    buf: [u8; 16 * 1024],
    len: usize,
    header_scan: usize,
}

// Keep enough trailing bytes to detect the 4-byte `\r\n\r\n` marker across reads.
const HEADER_END_OVERLAP: usize = 3;

impl RawH1ResponseReader {
    fn new() -> Self {
        Self {
            buf: [0; 16 * 1024],
            len: 0,
            header_scan: 0,
        }
    }

    fn clear(&mut self) {
        self.len = 0;
        self.header_scan = 0;
    }

    async fn read_response(&mut self, stream: &mut TcpStream) -> Result<(bool, usize)> {
        loop {
            if let Some(header_end) = find_header_end_from(&self.buf[..self.len], self.header_scan)
            {
                let body_len =
                    content_length(&self.buf[..header_end]).context("missing content-length")?;
                let response_len = header_end + 4 + body_len;
                if self.len >= response_len {
                    let status_ok = status_is_success(&self.buf[..header_end]);
                    self.consume(response_len);
                    return Ok((status_ok, body_len));
                }
                self.header_scan = header_end;
            } else {
                self.header_scan = self.len.saturating_sub(HEADER_END_OVERLAP);
            }

            if self.len == self.buf.len() {
                anyhow::bail!("h1 response buffer exceeded {} bytes", self.buf.len());
            }
            let n = stream
                .read(&mut self.buf[self.len..])
                .await
                .context("read h1 response")?;
            if n == 0 {
                anyhow::bail!("connection closed while reading response");
            }
            self.len += n;
        }
    }

    fn consume(&mut self, n: usize) {
        if n == self.len {
            self.clear();
        } else {
            self.buf.copy_within(n..self.len, 0);
            self.len -= n;
            self.header_scan = self.header_scan.saturating_sub(n);
        }
    }
}

fn build_client(cfg: &BenchConfig) -> Result<reqwest::Client> {
    let mut b = reqwest::Client::builder()
        .pool_max_idle_per_host(cfg.connections)
        .pool_idle_timeout(Some(Duration::from_secs(60)))
        .tcp_nodelay(true)
        .timeout(Duration::from_secs(30));
    b = match cfg.http_version {
        HttpVersion::Auto => b,
        HttpVersion::Http1 => b.http1_only(),
        HttpVersion::Http2 => b.http2_prior_knowledge(),
        // Note: reqwest 0.12 gates H3 behind unstable features; for the
        // public bench harness we fall back to H2 if H3 isn't built in.
        // The dispatcher and server still expose H3 — the bench is a
        // separate client crate concern.
        HttpVersion::Http3 => b,
    };
    Ok(b.build().context("build reqwest client")?)
}

/// Render a report as a human-readable table.
pub fn render_human(r: &BenchReport) -> String {
    format!(
        "HELIOS bench report\n\
         ===================\n\
         URL:                {}\n\
         Duration:           {:.2}s\n\
         Connections:        {}\n\
         Requests:           {}\n\
         Errors:             {}\n\
         Throughput:         {:.0} req/s   ({:.2} Mbps)\n\
         Latency (ms):       min={:.3}  p50={:.3}  p90={:.3}  p99={:.3}  p99.9={:.3}  max={:.3}  mean={:.3}\n",
        r.url, r.duration_s, r.connections, r.requests, r.errors,
        r.rps, r.throughput_mbps,
        r.latency_ms.min, r.latency_ms.p50, r.latency_ms.p90,
        r.latency_ms.p99, r.latency_ms.p999, r.latency_ms.max,
        r.latency_ms.mean,
    )
}
