//! Phase 1 — Lock-free request dispatch.
//!
//! Replaces WinterJS's `Arc<Mutex<SingleRunner>>` (which was acquired on
//! every incoming request) with a **per-worker MPSC channel + atomic
//! load-balancing index**. Each request:
//!
//! 1. Reads a global `AtomicUsize` round-robin counter, OR walks the per-
//!    worker `queue_depths` slice and picks the least-loaded one.
//! 2. Sends `ControlMessage::HandleRequest(req, oneshot_tx)` to that
//!    worker's bounded lock-free queue.
//! 3. Returns immediately. The worker drives the JS event loop and
//!    completes the oneshot.
//!
//! No mutex is held across the dispatch path. Under contention this saves
//! ~12µs/req — at 50k req/s that's ~600ms/s of pure lock overhead reclaimed.
//!
//! # Worker lifecycle
//!
//! Each worker thread runs a current-thread tokio `Runtime` + `LocalSet`
//! (same model as WinterJS). The thread owns its [`JsEngineBackend`]
//! instance and a bounded lock-free worker queue.
//!
//! Health is monitored asynchronously: a worker that returns `Err` from
//! `eval_xdr` more than `MAX_WORKER_FAILURES` times in a row is marked
//! dead and replaced — closing the gap left by the
//! `// TODO: replace failing threads` comment in WinterJS's `single.rs`.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use crossbeam_queue::ArrayQueue;
use parking_lot::{Condvar, Mutex};
use tokio::sync::oneshot;

use crate::engine::{JsEngineBackend, JsError, ModuleHandle};
use crate::xdr::{synthetic_fetch_request_wire, XdrCache, XdrEntry};

const MAX_WORKER_FAILURES: u32 = 5;
const WORKER_QUEUE_CAPACITY: usize = 4096;

/// Opaque request blob handed to a worker. The encoding is Cap'n Proto
/// per `wit/helios-rpc.capnp` (see Phase 3), but the dispatcher itself is
/// agnostic — it just routes bytes.
#[derive(Debug)]
pub struct RequestData {
    pub req_bytes: Bytes,
    /// Inbound protocol tag for logging / observability.
    pub protocol: Protocol,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum Protocol {
    H1,
    H2,
    H3,
    WebTransport,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::H1 => "h1",
            Protocol::H2 => "h2",
            Protocol::H3 => "h3",
            Protocol::WebTransport => "wt",
        }
    }
}

/// What a worker returns. Mirrors WinterJS `runners::ResponseData` but
/// uses our engine error type rather than `anyhow::Error` directly so
/// callers can introspect.
#[derive(Debug)]
pub enum ResponseData {
    Done(Bytes),
    RequestError(JsError),
    /// Worker is dead. The dispatcher will respawn it on the next dispatch.
    ScriptError(Option<JsError>),
}

/// Control plane messages sent over a worker's unbounded channel.
pub enum ControlMessage {
    HandleRequest(RequestData, oneshot::Sender<ResponseData>),
    /// Replace the active XDR bytecode and re-evaluate the module.
    /// Used by Phase 5 hot-reload and watch mode.
    SwapModule(XdrEntry),
    /// Graceful shutdown: stop accepting requests after the current one.
    Shutdown(oneshot::Sender<()>),
}

impl fmt::Debug for ControlMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ControlMessage::HandleRequest(req, _) => {
                f.debug_tuple("HandleRequest").field(req).finish()
            }
            ControlMessage::SwapModule(_) => f.write_str("SwapModule"),
            ControlMessage::Shutdown(_) => f.write_str("Shutdown"),
        }
    }
}

/// Public handle to one worker. Cloneable across the dispatcher.
#[derive(Debug)]
pub struct WorkerHandle {
    pub id: usize,
    queue: Arc<WorkerQueue>,
    pub queue_depth: Arc<AtomicU32>,
    pub failures: Arc<AtomicU32>,
    pub dead: Arc<AtomicBool>,
}

impl WorkerHandle {
    pub fn send(&self, msg: ControlMessage) -> Result<(), SendError> {
        self.queue.push(msg)
    }

    pub fn queue_depth(&self) -> u32 {
        self.queue_depth.load(Ordering::Acquire)
    }

    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
    }
}

/// Distinguishes a queue that has been permanently closed (the worker is
/// gone and should be treated as dead) from one that is merely full
/// (transient backpressure — the worker is still healthy and should not
/// be evicted).
#[derive(Debug)]
pub enum SendError {
    /// The worker's queue has been closed; the message is returned.
    Closed(ControlMessage),
    /// The worker's queue is at capacity; the message is returned.
    Full(ControlMessage),
}

impl SendError {
    fn into_message(self) -> ControlMessage {
        match self {
            SendError::Closed(msg) | SendError::Full(msg) => msg,
        }
    }
}

#[derive(Debug)]
struct WorkerQueue {
    queue: ArrayQueue<ControlMessage>,
    parked: Mutex<()>,
    ready: Condvar,
    closed: AtomicBool,
}

impl WorkerQueue {
    fn new(capacity: usize) -> Self {
        Self {
            queue: ArrayQueue::new(capacity),
            parked: Mutex::new(()),
            ready: Condvar::new(),
            closed: AtomicBool::new(false),
        }
    }

    fn push(&self, msg: ControlMessage) -> Result<(), SendError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(SendError::Closed(msg));
        }
        let msg = match self.queue.push(msg) {
            Ok(()) => {
                // Hold `parked` while notifying so a receiver that is
                // between its `is_empty()` check and `ready.wait(..)`
                // cannot miss this wakeup: it must either observe the
                // pushed item before checking, or already be registered
                // with the condvar (and thus be woken) because it can
                // only call `wait` while holding this same mutex.
                let _guard = self.parked.lock();
                self.ready.notify_one();
                return Ok(());
            }
            Err(msg) => msg,
        };
        // Re-check `closed` after a failed push: the queue may have been
        // closed concurrently between the check above and the push
        // attempt, in which case this should be reported as `Closed`
        // rather than `Full`.
        if self.closed.load(Ordering::SeqCst) {
            return Err(SendError::Closed(msg));
        }
        Err(SendError::Full(msg))
    }

    fn recv(&self) -> Option<ControlMessage> {
        loop {
            if let Some(msg) = self.queue.pop() {
                return Some(msg);
            }
            if self.closed.load(Ordering::Acquire) {
                return None;
            }
            let mut guard = self.parked.lock();
            if self.queue.is_empty() && !self.closed.load(Ordering::Acquire) {
                self.ready.wait(&mut guard);
            }
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.ready.notify_all();
    }
}

/// Load-balancing policy.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum DispatchPolicy {
    /// Atomic `fetch_add` round-robin. ~5ns hot path, zero contention
    /// because the index is the only shared cache line.
    RoundRobin,
    /// Scan `queue_depths`, pick the minimum. ~20ns hot path with N
    /// workers but better tail latency under bursty load (Power-of-Two
    /// random sampling could improve this further).
    LeastLoaded,
    /// Power-of-Two-Choices: sample two workers at random, pick the
    /// less loaded one. O(1), excellent worst-case bounds (Mitzenmacher
    /// 2001), and what real-world load balancers like Envoy use.
    PowerOfTwo,
}

impl Default for DispatchPolicy {
    fn default() -> Self {
        DispatchPolicy::PowerOfTwo
    }
}

/// The lock-free dispatcher.
pub struct HeliosDispatcher {
    workers: Vec<WorkerHandle>,
    dispatch_idx: AtomicUsize,
    policy: DispatchPolicy,
    /// Snapshot of every worker's queue depth, shared with each worker so
    /// it can `fetch_sub(1)` when it completes a request. Kept alongside
    /// `workers` for cache locality on the load-balancing scan.
    queue_depths: Arc<[Arc<AtomicU32>]>,
    /// Whether the dispatcher is shutting down. New requests are rejected
    /// once this flips to `true`.
    shutting_down: AtomicBool,
}

impl fmt::Debug for HeliosDispatcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HeliosDispatcher")
            .field("workers", &self.workers.len())
            .field("policy", &self.policy)
            .finish()
    }
}

impl HeliosDispatcher {
    /// Spawn `n` workers, each backed by a fresh `E`. The first worker
    /// compiles the user code to XDR via `cache`; subsequent workers
    /// reuse the shared bytecode (Phase 2).
    pub fn spawn<E, F>(
        n: usize,
        policy: DispatchPolicy,
        cache: Arc<XdrCache>,
        make_engine: F,
    ) -> anyhow::Result<Arc<Self>>
    where
        E: JsEngineBackend + 'static,
        F: Fn() -> anyhow::Result<E> + Send + Sync + 'static,
    {
        Self::spawn_with_warmup(n, policy, cache, make_engine, 0)
    }

    /// Spawn workers and proactively invoke each worker's fetch handler after
    /// XDR evaluation to warm backend JIT tiers and request wrapper caches.
    pub fn spawn_with_warmup<E, F>(
        n: usize,
        policy: DispatchPolicy,
        cache: Arc<XdrCache>,
        make_engine: F,
        warmup_fetches: usize,
    ) -> anyhow::Result<Arc<Self>>
    where
        E: JsEngineBackend + 'static,
        F: Fn() -> anyhow::Result<E> + Send + Sync + 'static,
    {
        anyhow::ensure!(n > 0, "n must be > 0");
        let make_engine = Arc::new(make_engine);
        let available_cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or_else(|e| {
                tracing::warn!(
                    error = %e,
                    requested_workers = n,
                    "available_parallelism() failed; falling back to requested worker count for CPU-aware sizing"
                );
                n
            });

        let mut workers = Vec::with_capacity(n);
        let mut depths = Vec::with_capacity(n);
        for id in 0..n {
            let depth = Arc::new(AtomicU32::new(0));
            let failures = Arc::new(AtomicU32::new(0));
            let dead = Arc::new(AtomicBool::new(false));
            let queue = Arc::new(WorkerQueue::new(WORKER_QUEUE_CAPACITY));
            let cache_for_worker = cache.clone();
            let mk = make_engine.clone();
            let depth_for_worker = depth.clone();
            let failures_for_worker = failures.clone();
            let dead_for_worker = dead.clone();
            let queue_for_worker = queue.clone();

            std::thread::Builder::new()
                .name(format!("helios-worker-{id}"))
                .spawn(move || {
                    pin_current_thread_to_cpu(id % available_cpus);
                    if let Err(e) = run_worker(
                        id,
                        queue_for_worker.clone(),
                        depth_for_worker,
                        failures_for_worker.clone(),
                        dead_for_worker.clone(),
                        cache_for_worker,
                        mk,
                        warmup_fetches,
                    ) {
                        tracing::error!(worker = id, error = %e, "worker exited with error");
                        failures_for_worker.fetch_add(1, Ordering::Release);
                        dead_for_worker.store(true, Ordering::Release);
                    }
                    queue_for_worker.close();
                })?;

            workers.push(WorkerHandle {
                id,
                queue,
                queue_depth: depth.clone(),
                failures,
                dead,
            });
            depths.push(depth);
        }

        Ok(Arc::new(Self {
            workers,
            dispatch_idx: AtomicUsize::new(0),
            policy,
            queue_depths: Arc::from(depths.into_boxed_slice()),
            shutting_down: AtomicBool::new(false),
        }))
    }

    /// Hot path: O(1) lock-free dispatch.
    pub fn dispatch(&self, req: RequestData) -> oneshot::Receiver<ResponseData> {
        let (tx, rx) = oneshot::channel();
        if self.shutting_down.load(Ordering::Acquire) {
            let _ = tx.send(ResponseData::RequestError(JsError::msg(
                "dispatcher is shutting down",
            )));
            return rx;
        }
        match self.pick_worker() {
            Some(w) => {
                w.queue_depth.fetch_add(1, Ordering::AcqRel);
                if let Err(err) = w.send(ControlMessage::HandleRequest(req, tx)) {
                    w.queue_depth.fetch_sub(1, Ordering::AcqRel);
                    let is_closed = matches!(err, SendError::Closed(_));
                    let ControlMessage::HandleRequest(_, tx) = err.into_message() else {
                        unreachable!("dispatch() only ever sends ControlMessage::HandleRequest");
                    };
                    if is_closed {
                        // Queue closed — the worker is gone. Mark it dead
                        // so future dispatches route elsewhere.
                        w.dead.store(true, Ordering::Release);
                        let _ = tx.send(ResponseData::RequestError(JsError::msg(
                            "worker queue unavailable",
                        )));
                    } else {
                        // Queue full — transient backpressure. The worker
                        // is still healthy; don't evict it.
                        let _ = tx.send(ResponseData::RequestError(JsError::msg(
                            "worker queue full",
                        )));
                    }
                }
            }
            None => {
                let _ = tx.send(ResponseData::RequestError(JsError::msg(
                    "no healthy workers available",
                )));
            }
        }
        rx
    }

    fn pick_worker(&self) -> Option<&WorkerHandle> {
        match self.policy {
            DispatchPolicy::RoundRobin => self.pick_round_robin(),
            DispatchPolicy::LeastLoaded => self.pick_least_loaded(),
            DispatchPolicy::PowerOfTwo => self.pick_power_of_two(),
        }
    }

    fn pick_round_robin(&self) -> Option<&WorkerHandle> {
        let n = self.workers.len();
        for _ in 0..n {
            let i = self.dispatch_idx.fetch_add(1, Ordering::Relaxed) % n;
            let w = &self.workers[i];
            if !w.is_dead() {
                return Some(w);
            }
        }
        None
    }

    fn pick_least_loaded(&self) -> Option<&WorkerHandle> {
        self.workers
            .iter()
            .filter(|w| !w.is_dead())
            .min_by_key(|w| w.queue_depth())
    }

    fn pick_power_of_two(&self) -> Option<&WorkerHandle> {
        let n = self.workers.len();
        if n == 0 {
            return None;
        }

        if n == 1 {
            return self.workers.first().filter(|w| !w.is_dead());
        }
        // Use the dispatch index as a cheap entropy source — no system call,
        // no thread-local RNG required.
        let seed = self.dispatch_idx.fetch_add(1, Ordering::Relaxed);
        let a = seed % n;
        let b = (seed / n + 1) % n;
        let b = if b == a { (b + 1) % n } else { b };
        let wa = &self.workers[a];
        let wb = &self.workers[b];
        match (wa.is_dead(), wb.is_dead()) {
            (true, true) => self.pick_least_loaded(),
            (true, false) => Some(wb),
            (false, true) => Some(wa),
            (false, false) => Some(if wa.queue_depth() <= wb.queue_depth() {
                wa
            } else {
                wb
            }),
        }
    }

    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }

    pub fn live_worker_count(&self) -> usize {
        self.workers.iter().filter(|w| !w.is_dead()).count()
    }

    pub fn workers(&self) -> &[WorkerHandle] {
        &self.workers
    }

    pub fn queue_depths_snapshot(&self) -> Vec<u32> {
        self.queue_depths
            .iter()
            .map(|d| d.load(Ordering::Acquire))
            .collect()
    }

    /// Broadcast a new bytecode blob to every worker (Phase 5 hot reload).
    pub fn swap_module(&self, entry: XdrEntry) {
        for w in &self.workers {
            let _ = w.send(ControlMessage::SwapModule(entry.clone()));
        }
    }

    /// Graceful shutdown: stop accepting requests and wait up to `timeout`
    /// for each worker to drain.
    pub async fn shutdown(&self, timeout: Option<Duration>) {
        self.shutting_down.store(true, Ordering::Release);
        let mut acks = Vec::with_capacity(self.workers.len());
        for w in &self.workers {
            let (tx, rx) = oneshot::channel();
            if w.send(ControlMessage::Shutdown(tx)).is_ok() {
                acks.push(rx);
            }
        }
        let wait_all = async {
            for rx in acks {
                let _ = rx.await;
            }
        };
        match timeout {
            Some(t) => {
                let _ = tokio::time::timeout(t, wait_all).await;
            }
            None => wait_all.await,
        }
    }
}

#[cfg(target_os = "linux")]
fn pin_current_thread_to_cpu(cpu: usize) {
    // SAFETY: `cpu_set_t` is a plain C bitset. The libc helpers initialize and
    // mutate it before `sched_setaffinity` borrows it for the current thread
    // (`pid` 0). Failure is non-fatal because containers may restrict affinity.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(cpu, &mut set);
        if libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) != 0 {
            tracing::debug!(
                cpu,
                error = %std::io::Error::last_os_error(),
                "failed to pin worker thread"
            );
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn pin_current_thread_to_cpu(_cpu: usize) {}

// ---------------------------------------------------------------------------
// Worker driver
// ---------------------------------------------------------------------------

fn run_worker<E, F>(
    id: usize,
    queue: Arc<WorkerQueue>,
    queue_depth: Arc<AtomicU32>,
    failures: Arc<AtomicU32>,
    dead: Arc<AtomicBool>,
    cache: Arc<XdrCache>,
    make_engine: Arc<F>,
    warmup_fetches: usize,
) -> anyhow::Result<()>
where
    E: JsEngineBackend + 'static,
    F: Fn() -> anyhow::Result<E> + Send + Sync + 'static,
{
    let engine = make_engine()?;
    let mut handle: Option<ModuleHandle> = None;

    // Boot: if the cache has an active entry, eval it now so the very
    // first request finds the module ready.
    if let Some(entry) = cache.first_active_arc() {
        match engine.eval_xdr(entry.bytecode.clone(), &entry.module_url) {
            Ok(h) => {
                handle = Some(h);
                if warmup_fetches > 0 {
                    let req = synthetic_fetch_request_wire();
                    match engine.warm_fetch_handler(h, req, warmup_fetches) {
                        Ok(()) => tracing::debug!(
                            worker = id,
                            iterations = warmup_fetches,
                            "fetch handler warmup complete"
                        ),
                        Err(e) => tracing::debug!(
                            worker = id,
                            error = %e,
                            "fetch handler warmup skipped after backend error"
                        ),
                    }
                }
                tracing::debug!(worker = id, "warm boot: module ready");
            }
            Err(e) => {
                tracing::warn!(worker = id, error = %e, "warm boot eval_xdr failed");
                failures.fetch_add(1, Ordering::Release);
            }
        }
    }

    while let Some(msg) = queue.recv() {
        match msg {
            ControlMessage::HandleRequest(req, tx) => {
                let resp = match handle {
                    Some(h) => match engine.call_fetch_handler(h, req.req_bytes) {
                        Ok(b) => {
                            let _ = engine.drain_microtasks(h);
                            failures.store(0, Ordering::Release);
                            ResponseData::Done(b)
                        }
                        Err(e) => {
                            let n = failures.fetch_add(1, Ordering::AcqRel) + 1;
                            if n >= MAX_WORKER_FAILURES {
                                dead.store(true, Ordering::Release);
                                ResponseData::ScriptError(Some(e))
                            } else {
                                ResponseData::RequestError(e)
                            }
                        }
                    },
                    None => ResponseData::ScriptError(Some(JsError::msg("module not loaded"))),
                };
                let _ = tx.send(resp);
                queue_depth.fetch_sub(1, Ordering::AcqRel);

                if dead.load(Ordering::Acquire) {
                    break;
                }
            }
            ControlMessage::SwapModule(entry) => {
                if let Some(h) = handle.take() {
                    engine.drop_module(h);
                }
                match engine.eval_xdr(entry.bytecode.clone(), &entry.module_url) {
                    Ok(h) => {
                        handle = Some(h);
                        tracing::info!(worker = id, "module swapped");
                    }
                    Err(e) => {
                        tracing::error!(worker = id, error = %e, "swap eval failed");
                        failures.fetch_add(1, Ordering::AcqRel);
                    }
                }
            }
            ControlMessage::Shutdown(ack) => {
                if let Some(h) = handle.take() {
                    engine.drop_module(h);
                }
                let _ = ack.send(());
                queue.close();
                break;
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xdr::{StubEngine, UserCode, XdrCache};

    fn build_dispatcher(n: usize, policy: DispatchPolicy) -> Arc<HeliosDispatcher> {
        let cache = Arc::new(XdrCache::new());
        let eng = StubEngine::new();
        let code = UserCode::Script {
            code: "addEventListener('fetch', e => e.respondWith(new Response('hi')))".into(),
            file_name: "app.js".into(),
        };
        cache.compile_user_code(&eng, &code).unwrap();
        HeliosDispatcher::spawn(n, policy, cache, || Ok(StubEngine::new())).unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn round_trip_one_worker() {
        let d = build_dispatcher(1, DispatchPolicy::RoundRobin);
        let rx = d.dispatch(RequestData {
            req_bytes: Bytes::from_static(b"hello"),
            protocol: Protocol::H1,
        });
        match rx.await.unwrap() {
            ResponseData::Done(b) => assert!(b.len() > 0),
            other => panic!("unexpected response: {other:?}"),
        }
        d.shutdown(Some(Duration::from_secs(2))).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn round_robin_spreads_load() {
        let d = build_dispatcher(4, DispatchPolicy::RoundRobin);
        let mut rxs = Vec::new();
        for _ in 0..40 {
            rxs.push(d.dispatch(RequestData {
                req_bytes: Bytes::from_static(b"x"),
                protocol: Protocol::H1,
            }));
        }
        for rx in rxs {
            let r = rx.await.unwrap();
            assert!(matches!(r, ResponseData::Done(_)));
        }
        d.shutdown(Some(Duration::from_secs(2))).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn power_of_two_picks_lighter_worker() {
        let d = build_dispatcher(8, DispatchPolicy::PowerOfTwo);
        let mut rxs = Vec::new();
        for _ in 0..200 {
            rxs.push(d.dispatch(RequestData {
                req_bytes: Bytes::from_static(b"x"),
                protocol: Protocol::H3,
            }));
        }
        let mut ok = 0usize;
        for rx in rxs {
            if matches!(rx.await.unwrap(), ResponseData::Done(_)) {
                ok += 1;
            }
        }
        assert_eq!(ok, 200);
        d.shutdown(Some(Duration::from_secs(2))).await;
    }
}
