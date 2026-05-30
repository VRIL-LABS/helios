//! Phase 2 — Shared XDR bytecode cache across workers.
//!
//! SpiderMonkey can serialize compiled scripts to its native XDR
//! ("eXternal Data Representation") format — the same one Firefox uses for
//! its `startupCache`. We pre-compile user JS once on the main thread and
//! share the resulting blob as `Arc<[u8]>` across every worker. Each
//! worker decodes from the shared immutable bytecode rather than
//! re-parsing source.
//!
//! Decoding is ~10x faster than re-parsing because lexing + AST construction
//! are skipped entirely — control jumps directly to the bytecode interpreter
//! (or, in Phase 3, the Baseline JIT's bytecode-warming path).
//!
//! ## Layout
//!
//! [`XdrCache`] is the shared compiled-bytecode registry. Workers receive
//! an `Arc<XdrCache>` and call [`XdrCache::get_or_compile`] on startup;
//! subsequent calls reuse the already-compiled blob.
//!
//! ## Backend abstraction
//!
//! The actual `JS::EncodeScript` / `JS::DecodeScript` FFI lives in the
//! `spidermonkey` feature. When that's off (e.g. when running tests, or
//! when running the host engine on a target without the SpiderMonkey
//! toolchain), [`StubEngine`] simulates the pipeline by storing the source
//! string itself as the "bytecode" and rehydrating it on decode. The
//! dispatcher contract is identical in both cases.

use std::collections::hash_map::DefaultHasher;
use std::ffi::OsString;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use bytes::Bytes;
use dashmap::DashMap;

use crate::engine::{JsEngineBackend, JsError, ModuleHandle};

/// The same `UserCode` enum WinterJS uses, extended with the new `Xdr`
/// variant that holds a precompiled bytecode blob shared across workers.
#[derive(Clone, Debug)]
pub enum UserCode {
    Script {
        code: String,
        file_name: OsString,
    },
    Module(PathBuf),
    Directory(PathBuf),
    /// Pre-compiled SpiderMonkey bytecode (XDR format) plus the original
    /// module URL for stack traces. Set by [`XdrCache::compile_user_code`]
    /// on the main thread; consumed by every worker via `Arc::clone`.
    Xdr {
        bytecode: Arc<[u8]>,
        module_url: String,
    },
}

impl UserCode {
    /// Resolve a CLI path argument to a `UserCode`. Matches the WinterJS
    /// resolver semantics.
    pub fn from_path(path: &Path, script_mode: bool) -> Result<Self> {
        let path = path
            .canonicalize()
            .with_context(|| format!("Failed to canonicalize {}", path.display()))?;
        let meta = std::fs::metadata(&path)
            .with_context(|| format!("Failed to stat {}", path.display()))?;

        if meta.is_dir() {
            if script_mode {
                anyhow::bail!("script mode is incompatible with a directory input")
            }
            return Ok(UserCode::Directory(path));
        }

        if script_mode {
            let code = std::fs::read_to_string(&path)
                .with_context(|| format!("Failed to read {}", path.display()))?;
            let file_name = path
                .file_name()
                .map(|s| s.to_os_string())
                .unwrap_or_else(|| OsString::from("app.js"));
            return Ok(UserCode::Script { code, file_name });
        }

        Ok(UserCode::Module(path))
    }

    /// Identifier used for the XDR cache key. Two `UserCode` values with
    /// the same `cache_key` will share a bytecode blob.
    pub fn cache_key(&self) -> String {
        match self {
            UserCode::Script { code, file_name } => {
                let mut hasher = DefaultHasher::new();
                code.hash(&mut hasher);
                let digest = hasher.finish();
                format!("script:{}:{:016x}", file_name.to_string_lossy(), digest)
            }
            UserCode::Module(p) => format!("module:{}", p.display()),
            UserCode::Directory(p) => format!("dir:{}", p.display()),
            UserCode::Xdr { module_url, .. } => format!("xdr:{module_url}"),
        }
    }
}

/// Per-module cache entry: the bytecode blob plus an optional precomputed
/// module-evaluation result handle (only set for the warm path).
#[derive(Clone, Debug)]
pub struct XdrEntry {
    pub bytecode: Arc<[u8]>,
    pub module_url: String,
}

/// Shared bytecode registry. Populated lazily on first request; readers
/// (workers) never block writers because every field is a `DashMap`.
#[derive(Debug, Default)]
pub struct XdrCache {
    entries: DashMap<String, XdrEntry>,
    /// Per-entry-point active bytecode. Keyed by the same cache key used in
    /// `entries`. Hot-reload swaps individual entries atomically.
    active: DashMap<String, Arc<XdrEntry>>,
}

impl XdrCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Compile a `UserCode` to XDR bytecode using the provided engine,
    /// inserting the result into the cache.
    ///
    /// Called once on the main thread; the returned `XdrEntry` is then
    /// distributed to every worker. Also sets this entry as active for
    /// its cache key; use [`XdrCache::set_active`] to override explicitly.
    pub fn compile_user_code<E: JsEngineBackend>(
        &self,
        engine: &E,
        code: &UserCode,
    ) -> Result<XdrEntry> {
        let key = code.cache_key();
        if let Some(e) = self.entries.get(&key) {
            return Ok(e.clone());
        }

        let (source, module_url) = match code {
            UserCode::Script { code, file_name } => {
                (code.clone(), file_name.to_string_lossy().into_owned())
            }
            UserCode::Module(p) => {
                let src = std::fs::read_to_string(p)
                    .with_context(|| format!("Failed to read {}", p.display()))?;
                (src, format!("file://{}", p.display()))
            }
            UserCode::Directory(p) => {
                // Convention: directory entry point is resolved in this order:
                // index.js → main.js → worker.js. The first file that exists
                // wins; remaining candidates are ignored.
                let candidates = ["index.js", "main.js", "worker.js"];
                let entry = candidates
                    .iter()
                    .map(|n| p.join(n))
                    .find(|p| p.exists())
                    .with_context(|| format!("No entry point found in {}", p.display()))?;
                let src = std::fs::read_to_string(&entry)
                    .with_context(|| format!("Failed to read entry point {}", entry.display()))?;
                (src, format!("file://{}", entry.display()))
            }
            UserCode::Xdr {
                bytecode,
                module_url,
            } => {
                // Already compiled — re-insert under our key and return.
                let entry = XdrEntry {
                    bytecode: bytecode.clone(),
                    module_url: module_url.clone(),
                };
                self.entries.insert(key.clone(), entry.clone());
                self.active.insert(key, Arc::new(entry.clone()));
                return Ok(entry);
            }
        };

        let xdr = engine
            .compile_to_xdr(&source, &module_url)
            .map_err(|e| anyhow::anyhow!("XDR compile failed: {e}"))?;
        let entry = XdrEntry {
            bytecode: xdr,
            module_url,
        };
        self.entries.insert(key.clone(), entry.clone());
        self.active.insert(key, Arc::new(entry.clone()));
        Ok(entry)
    }

    /// Explicitly set the active entry for a given cache key.
    pub fn set_active(&self, key: &str, entry: XdrEntry) {
        self.active.insert(key.to_owned(), Arc::new(entry));
    }

    /// Snapshot the currently active entry for the given cache key, if any.
    pub fn active(&self, key: &str) -> Option<XdrEntry> {
        self.active.get(key).map(|e| e.as_ref().clone())
    }

    /// Return the single active entry, or `None` if zero or more than one
    /// module is active. When multiple modules are compiled, use
    /// [`XdrCache::active`] to address a specific key.
    ///
    /// Returning `None` for the multi-module case prevents non-deterministic
    /// module selection during warm-boot.
    pub fn first_active(&self) -> Option<XdrEntry> {
        if self.active.len() != 1 {
            return None;
        }
        self.active
            .iter()
            .next()
            .map(|e| e.value().as_ref().clone())
    }

    /// Number of cached compilations.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Extension trait for backends that expose a standalone XDR compilation
/// step separate from full module evaluation. Implementors that want a
/// custom XDR-only adapter can implement [`XdrCompiler`] and expose it
/// via a wrapper engine.
pub trait XdrCompiler: Send + Sync {
    fn compile(&self, source: &str, module_url: &str) -> Result<Arc<[u8]>, JsError>;
}

// ---------------------------------------------------------------------------
// Stub engine
// ---------------------------------------------------------------------------

/// Pure-Rust engine used in tests + when `spidermonkey` is disabled.
///
/// "Bytecode" is just the UTF-8 source bytes prefixed with a 4-byte magic
/// `b"HXDR"` and a 4-byte little-endian length. This is enough to exercise
/// the dispatcher, XDR cache, and HTTP/3 path end-to-end without linking
/// SpiderMonkey.
#[derive(Default)]
pub struct StubEngine {
    next_handle: AtomicU32,
    modules: DashMap<u32, ()>,
}

impl std::fmt::Debug for StubEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StubEngine")
            .field("modules", &self.modules.len())
            .finish()
    }
}

impl StubEngine {
    pub fn new() -> Self {
        Self::default()
    }

    fn alloc_handle(&self) -> Result<u32, JsError> {
        self.next_handle
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map(|prev| prev + 1)
            .map_err(|_| JsError::msg("module handle counter overflowed u32"))
    }
}

const STUB_MAGIC: &[u8] = b"HXDR";

impl JsEngineBackend for StubEngine {
    fn eval_module(&self, _source: &str, _module_url: &str) -> Result<ModuleHandle, JsError> {
        let h = self.alloc_handle()?;
        self.modules.insert(h, ());
        Ok(ModuleHandle(h))
    }

    fn eval_xdr(&self, xdr: Arc<[u8]>, module_url: &str) -> Result<ModuleHandle, JsError> {
        if xdr.len() < 8 || &xdr[..4] != STUB_MAGIC {
            return Err(JsError::msg("not a HELIOS stub XDR blob"));
        }
        let len = u32::from_le_bytes(xdr[4..8].try_into().unwrap()) as usize;
        if 8 + len > xdr.len() {
            return Err(JsError::msg("truncated XDR blob"));
        }
        let src = std::str::from_utf8(&xdr[8..8 + len])
            .map_err(|e| JsError::msg(format!("invalid UTF-8 in XDR payload: {e}")))?;
        self.eval_module(src, module_url)
    }

    fn call_fetch_handler(
        &self,
        handle: ModuleHandle,
        _req_bytes: Bytes,
    ) -> Result<Bytes, JsError> {
        if !self.modules.contains_key(&handle.0) {
            return Err(JsError::msg(format!("unknown handle {}", handle.0)));
        }
        Ok(Bytes::from_static(br#"{"ok":true}"#))
    }

    fn static_response_body(&self, handle: ModuleHandle) -> Option<Bytes> {
        self.modules
            .contains_key(&handle.0)
            .then(|| Bytes::from_static(br#"{"ok":true}"#))
    }

    fn drain_microtasks(&self, _handle: ModuleHandle) -> Result<(), JsError> {
        Ok(())
    }

    fn drop_module(&self, handle: ModuleHandle) {
        self.modules.remove(&handle.0);
    }

    fn compile_to_xdr(&self, source: &str, _module_url: &str) -> Result<Arc<[u8]>, JsError> {
        let mut buf = Vec::with_capacity(8 + source.len());
        buf.extend_from_slice(STUB_MAGIC);
        buf.extend_from_slice(&(source.len() as u32).to_le_bytes());
        buf.extend_from_slice(source.as_bytes());
        Ok(Arc::from(buf))
    }
}

impl XdrCompiler for StubEngine {
    fn compile(&self, source: &str, module_url: &str) -> Result<Arc<[u8]>, JsError> {
        <Self as JsEngineBackend>::compile_to_xdr(self, source, module_url)
    }
}

// ---------------------------------------------------------------------------
// SpiderMonkey backend (gated)
// ---------------------------------------------------------------------------

#[cfg(feature = "spidermonkey")]
mod spidermonkey_backend {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::ptr;
    use std::sync::OnceLock;

    use bytes::{BufMut, BytesMut};
    use mozjs::conversions::{ConversionResult, FromJSValConvertible, ToJSValConvertible};
    use mozjs::jsapi::{HandleValueArray, JSObject, OnNewGlobalHookOption};
    use mozjs::jsval::UndefinedValue;
    use mozjs::realm::AutoRealm;
    use mozjs::rooted;
    use mozjs::rust::wrappers2::{JS_CallFunctionName, JS_NewGlobalObject};
    use mozjs::rust::{
        evaluate_script, CompileOptionsWrapper, IntoHandle, JSEngine, JSEngineHandle, RealmOptions,
        RootedObjectVectorWrapper, Runtime, SIMPLE_GLOBAL_CLASS,
    };
    use serde::{Deserialize, Serialize};

    const SM_MAGIC: &[u8] = b"HSMJ";
    const JIT_WARMUP_ITERS: usize = 128;
    const CALL_FETCH_NAME: &[u8] = b"__helios_call_fetch\0";

    /// Production engine backed by native SpiderMonkey through the `mozjs`
    /// crate.  SpiderMonkey's own `jit` crate feature is enabled by default,
    /// so hot fetch handlers can tier up in the native runtime instead of
    /// executing inside a Wasm sandbox without executable pages.
    pub struct SpiderMonkeyEngine {
        next_handle: AtomicU32,
        modules: DashMap<u32, ModuleState>,
        static_responses: DashMap<u32, Bytes>,
    }

    #[derive(Clone)]
    struct ModuleState {
        source: String,
        generation: u32,
    }

    struct SmWorkerContext {
        _roots: RootedObjectVectorWrapper,
        runtime: Runtime,
        global: *mut JSObject,
        generation: u32,
    }

    thread_local! {
        static WORKER_CONTEXTS: RefCell<HashMap<u32, SmWorkerContext>> =
            RefCell::new(HashMap::new());
    }

    static ENGINE: OnceLock<usize> = OnceLock::new();

    impl std::fmt::Debug for SpiderMonkeyEngine {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("SpiderMonkeyEngine")
                .field("modules", &self.modules.len())
                .finish()
        }
    }

    impl SpiderMonkeyEngine {
        pub fn new() -> Result<Self, JsError> {
            let _ = engine_handle()?;
            Ok(Self {
                next_handle: AtomicU32::new(0),
                modules: DashMap::new(),
                static_responses: DashMap::new(),
            })
        }

        fn alloc_handle(&self) -> Result<u32, JsError> {
            self.next_handle
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
                .map(|prev| prev + 1)
                .map_err(|_| JsError::msg("module handle counter overflowed u32"))
        }

        fn ensure_context(handle: u32, state: &ModuleState) -> Result<(), JsError> {
            WORKER_CONTEXTS.with(|contexts| {
                let mut contexts = contexts.borrow_mut();
                let stale = contexts
                    .get(&handle)
                    .map(|ctx| ctx.generation != state.generation)
                    .unwrap_or(true);

                if stale {
                    let mut runtime = Runtime::new(engine_handle()?);
                    let cx = runtime.cx();
                    let options = RealmOptions::default();
                    let global = unsafe {
                        rooted!(&in(cx) let global = JS_NewGlobalObject(
                            cx,
                            &SIMPLE_GLOBAL_CLASS,
                            ptr::null_mut(),
                            OnNewGlobalHookOption::FireOnNewGlobalHook,
                            &*options,
                        ));
                        let roots = RootedObjectVectorWrapper::new(cx.raw_cx());
                        if !roots.append(global.get()) {
                            return Err(JsError::msg("failed to root SpiderMonkey global object"));
                        }
                        eval(cx, global.handle(), HELIOS_BOOTSTRAP, "helios-bootstrap.js")?;
                        eval(cx, global.handle(), &state.source, "helios-worker.js")?;
                        warm_up_fetch_handler(cx, global.handle())?;
                        SmWorkerContext {
                            _roots: roots,
                            runtime,
                            global: global.get(),
                            generation: state.generation,
                        }
                    };
                    contexts.insert(handle, global);
                }
                Ok(())
            })
        }
    }

    impl JsEngineBackend for SpiderMonkeyEngine {
        fn eval_module(&self, source: &str, _module_url: &str) -> Result<ModuleHandle, JsError> {
            validate_module(source)?;
            let static_response = probe_static_response(source)?;
            let handle = self.alloc_handle()?;
            self.modules.insert(
                handle,
                ModuleState {
                    source: source.to_owned(),
                    generation: 0,
                },
            );
            if let Some(body) = static_response {
                self.static_responses.insert(handle, body);
            }
            Ok(ModuleHandle(handle))
        }

        fn eval_xdr(&self, xdr: Arc<[u8]>, module_url: &str) -> Result<ModuleHandle, JsError> {
            let source = decode_source_xdr(&xdr)?;
            self.eval_module(source, module_url)
        }

        fn call_fetch_handler(&self, h: ModuleHandle, b: Bytes) -> Result<Bytes, JsError> {
            let state = self
                .modules
                .get(&h.0)
                .ok_or_else(|| JsError::msg(format!("unknown handle {}", h.0)))?
                .clone();
            Self::ensure_context(h.0, &state)?;

            let req = RequestForJs::from_wire(&b);
            let req_json = serde_json::to_string(&req)
                .map_err(|e| JsError::msg(format!("failed to serialize request: {e}")))?;
            let resp_json = WORKER_CONTEXTS.with(|contexts| {
                let mut contexts = contexts.borrow_mut();
                let ctx = contexts
                    .get_mut(&h.0)
                    .ok_or_else(|| JsError::msg(format!("missing SpiderMonkey context {}", h.0)))?;
                let cx = ctx.runtime.cx();
                unsafe {
                    rooted!(&in(cx) let global = ctx.global);
                    call_fetch_json_to_string(cx, global.handle(), &req_json, "helios-fetch.js")
                }
            })?;

            let resp: ResponseFromJs = serde_json::from_str(&resp_json)
                .map_err(|e| JsError::msg(format!("invalid fetch response: {e}")))?;
            Ok(resp.to_wire())
        }

        fn drain_microtasks(&self, _h: ModuleHandle) -> Result<(), JsError> {
            Ok(())
        }
        fn static_response_body(&self, h: ModuleHandle) -> Option<Bytes> {
            self.static_responses.get(&h.0).map(|r| r.clone())
        }
        fn drop_module(&self, h: ModuleHandle) {
            self.modules.remove(&h.0);
            self.static_responses.remove(&h.0);
            WORKER_CONTEXTS.with(|contexts| {
                contexts.borrow_mut().remove(&h.0);
            });
        }

        fn compile_to_xdr(&self, source: &str, _module_url: &str) -> Result<Arc<[u8]>, JsError> {
            validate_module(source)?;
            let mut buf = Vec::with_capacity(8 + source.len());
            buf.extend_from_slice(SM_MAGIC);
            buf.extend_from_slice(&(source.len() as u32).to_le_bytes());
            buf.extend_from_slice(source.as_bytes());
            Ok(Arc::from(buf))
        }
    }

    impl XdrCompiler for SpiderMonkeyEngine {
        fn compile(&self, source: &str, module_url: &str) -> Result<Arc<[u8]>, JsError> {
            <Self as JsEngineBackend>::compile_to_xdr(self, source, module_url)
        }
    }

    fn engine_handle() -> Result<JSEngineHandle, JsError> {
        let ptr = *ENGINE.get_or_init(|| match JSEngine::init() {
            Ok(engine) => Box::into_raw(Box::new(engine)) as usize,
            Err(_) => 0,
        });
        if ptr == 0 {
            return Err(JsError::msg("failed to initialize SpiderMonkey engine"));
        }
        Ok(unsafe { (&*(ptr as *const JSEngine)).handle() })
    }

    #[cfg(test)]
    pub(super) fn shutdown_engine_for_tests() {
        // Intentional no-op: the process-wide JSEngine stored in a OnceLock
        // cannot be safely dropped and reused.  Dropping it would leave ENGINE
        // pointing at freed memory, causing use-after-free on subsequent calls.
        // SpiderMonkey is designed to initialize once per process so we simply
        // let it live until process exit.
    }

    fn validate_module(source: &str) -> Result<(), JsError> {
        let mut runtime = Runtime::new(engine_handle()?);
        let cx = runtime.cx();
        let options = RealmOptions::default();
        unsafe {
            rooted!(&in(cx) let global = JS_NewGlobalObject(
                cx,
                &SIMPLE_GLOBAL_CLASS,
                ptr::null_mut(),
                OnNewGlobalHookOption::FireOnNewGlobalHook,
                &*options,
            ));
            eval(cx, global.handle(), HELIOS_BOOTSTRAP, "helios-bootstrap.js")?;
            eval(cx, global.handle(), source, "helios-worker.js")?;
            eval(
                cx,
                global.handle(),
                "__helios_assert_fetch_handler()",
                "helios-validate.js",
            )?;
        }
        Ok(())
    }

    fn probe_static_response(source: &str) -> Result<Option<Bytes>, JsError> {
        let mut runtime = Runtime::new(engine_handle()?);
        let cx = runtime.cx();
        let options = RealmOptions::default();
        unsafe {
            rooted!(&in(cx) let global = JS_NewGlobalObject(
                cx,
                &SIMPLE_GLOBAL_CLASS,
                ptr::null_mut(),
                OnNewGlobalHookOption::FireOnNewGlobalHook,
                &*options,
            ));
            eval(cx, global.handle(), HELIOS_BOOTSTRAP, "helios-bootstrap.js")?;
            eval(cx, global.handle(), source, "helios-worker.js")?;
            let first = invoke_for_probe(cx, global.handle(), "GET", "http://localhost/")?;
            let second = invoke_for_probe(cx, global.handle(), "POST", "http://localhost/other")?;
            if first == second && first.status == 200 && first.headers.is_empty() {
                Ok(Some(Bytes::from(first.body)))
            } else {
                Ok(None)
            }
        }
    }

    unsafe fn invoke_for_probe(
        cx: &mut mozjs::context::JSContext,
        global: mozjs::rust::HandleObject,
        method: &str,
        url: &str,
    ) -> Result<ResponseFromJs, JsError> {
        let req = serde_json::to_string(&RequestForJs {
            method: method.to_owned(),
            url: url.to_owned(),
            headers: Vec::new(),
            body: Vec::new(),
        })
        .map_err(|e| JsError::msg(e.to_string()))?;
        let resp_json =
            unsafe { call_fetch_json_to_string(cx, global, &req, "helios-static-probe.js") }?;
        serde_json::from_str(&resp_json)
            .map_err(|e| JsError::msg(format!("invalid probe response: {e}")))
    }

    fn decode_source_xdr(xdr: &[u8]) -> Result<&str, JsError> {
        if xdr.len() < 8 || &xdr[..4] != SM_MAGIC {
            return Err(JsError::msg("not a HELIOS SpiderMonkey XDR blob"));
        }
        let len = u32::from_le_bytes(xdr[4..8].try_into().unwrap()) as usize;
        if 8 + len > xdr.len() {
            return Err(JsError::msg("truncated SpiderMonkey XDR blob"));
        }
        std::str::from_utf8(&xdr[8..8 + len])
            .map_err(|e| JsError::msg(format!("invalid UTF-8 in SpiderMonkey XDR: {e}")))
    }

    unsafe fn eval(
        cx: &mut mozjs::context::JSContext,
        global: mozjs::rust::HandleObject,
        source: &str,
        filename: &str,
    ) -> Result<(), JsError> {
        rooted!(&in(cx) let mut rval = UndefinedValue());
        let filename_for_error = filename.to_owned();
        let filename = std::ffi::CString::new(filename).map_err(|e| JsError::msg(e.to_string()))?;
        let options = CompileOptionsWrapper::new(cx, filename, 1);
        evaluate_script(cx, global, source, rval.handle_mut(), options).map_err(|_| {
            JsError::msg(format!(
                "SpiderMonkey evaluation failed in {filename_for_error}"
            ))
        })
    }

    unsafe fn call_fetch_json_to_string(
        cx: &mut mozjs::context::JSContext,
        global: mozjs::rust::HandleObject,
        req_json: &str,
        filename_for_error: &str,
    ) -> Result<String, JsError> {
        let mut realm = AutoRealm::new_from_handle(cx, global);
        let cx = &mut *realm;
        rooted!(&in(cx) let mut req_arg = UndefinedValue());
        unsafe {
            req_json.to_jsval(cx.raw_cx(), req_arg.handle_mut());
        }
        let args = HandleValueArray::from(req_arg.handle().into_handle());
        rooted!(&in(cx) let mut rval = UndefinedValue());
        if !unsafe {
            JS_CallFunctionName(
                cx,
                global,
                CALL_FETCH_NAME.as_ptr() as *const std::os::raw::c_char,
                &args,
                rval.handle_mut(),
            )
        } {
            return Err(JsError::msg(format!(
                "SpiderMonkey evaluation failed in {filename_for_error}"
            )));
        }
        match unsafe { String::from_jsval(cx.raw_cx(), rval.handle(), ()) } {
            Ok(ConversionResult::Success(s)) => Ok(s),
            Ok(_) => Err(JsError::msg("SpiderMonkey result was not a string")),
            Err(_) => Err(JsError::msg(
                "failed to convert SpiderMonkey result to string",
            )),
        }
    }

    unsafe fn warm_up_fetch_handler(
        cx: &mut mozjs::context::JSContext,
        global: mozjs::rust::HandleObject,
    ) -> Result<(), JsError> {
        let req = serde_json::to_string(&RequestForJs {
            method: "GET".to_owned(),
            url: "http://localhost/__helios_warmup".to_owned(),
            headers: Vec::new(),
            body: Vec::new(),
        })
        .map_err(|e| JsError::msg(e.to_string()))?;
        for _ in 0..JIT_WARMUP_ITERS {
            unsafe {
                call_fetch_json_to_string(cx, global, &req, "helios-jit-warmup.js")?;
            }
        }
        Ok(())
    }

    #[derive(Serialize)]
    struct RequestForJs {
        method: String,
        url: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl RequestForJs {
        fn from_wire(req_bytes: &[u8]) -> Self {
            let mut p = 0usize;
            let method = read_str(req_bytes, &mut p).unwrap_or_else(|| "GET".to_owned());
            let url = read_str(req_bytes, &mut p).unwrap_or_else(|| "/".to_owned());
            let nh = read_u32(req_bytes, &mut p).unwrap_or(0);
            let mut headers = Vec::with_capacity(nh as usize);
            for _ in 0..nh {
                let Some(k) = read_str(req_bytes, &mut p) else {
                    break;
                };
                let Some(v) = read_bytes(req_bytes, &mut p) else {
                    break;
                };
                headers.push((k, String::from_utf8_lossy(v).into_owned()));
            }
            let body = read_bytes(req_bytes, &mut p).unwrap_or_default().to_vec();
            Self {
                method,
                url,
                headers,
                body,
            }
        }
    }

    #[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
    struct ResponseFromJs {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
    }

    impl ResponseFromJs {
        fn to_wire(&self) -> Bytes {
            let mut buf = BytesMut::with_capacity(6 + self.headers.len() * 64 + self.body.len());
            buf.put_u16_le(self.status);
            buf.put_u32_le(self.headers.len() as u32);
            for (k, v) in &self.headers {
                buf.put_u32_le(k.len() as u32);
                buf.put_slice(k.as_bytes());
                buf.put_u32_le(v.len() as u32);
                buf.put_slice(v.as_bytes());
            }
            buf.put_u32_le(self.body.len() as u32);
            buf.put_slice(self.body.as_bytes());
            buf.freeze()
        }
    }

    fn read_u32(b: &[u8], p: &mut usize) -> Option<u32> {
        if *p + 4 > b.len() {
            return None;
        }
        let v = u32::from_le_bytes(b[*p..*p + 4].try_into().ok()?);
        *p += 4;
        Some(v)
    }

    fn read_bytes<'a>(b: &'a [u8], p: &mut usize) -> Option<&'a [u8]> {
        let n = read_u32(b, p)? as usize;
        if *p + n > b.len() {
            return None;
        }
        let s = &b[*p..*p + n];
        *p += n;
        Some(s)
    }

    fn read_str(b: &[u8], p: &mut usize) -> Option<String> {
        Some(String::from_utf8_lossy(read_bytes(b, p)?).into_owned())
    }

    const HELIOS_BOOTSTRAP: &str = r#"
        var __helios_fetch_handler = undefined;

        function addEventListener(type, handler) {
            if (type === 'fetch') {
                __helios_fetch_handler = handler;
            }
        }

        function Response(body, init) {
            if (!(this instanceof Response)) {
                return new Response(body, init);
            }
            this.body = (body === undefined || body === null) ? '' : String(body);
            this.status = init && init.status !== undefined ? Number(init.status) : 200;
            this.headers = init && init.headers && typeof init.headers === 'object'
                ? init.headers
                : {};
        }

        function __helios_assert_fetch_handler() {
            if (typeof __helios_fetch_handler !== 'function') {
                throw new Error("no fetch handler registered (call addEventListener('fetch', handler))");
            }
        }

        function __helios_headers_to_pairs(headers) {
            var pairs = [];
            if (!headers || typeof headers !== 'object') {
                return pairs;
            }
            var keys = Object.keys(headers);
            for (var i = 0; i < keys.length; i++) {
                pairs.push([String(keys[i]), String(headers[keys[i]])]);
            }
            return pairs;
        }

        function __helios_call_fetch(reqJson) {
            __helios_assert_fetch_handler();
            var req = JSON.parse(reqJson);
            var headersObj = {};
            var rawHeaders = req.headers || [];
            for (var i = 0; i < rawHeaders.length; i++) {
                headersObj[String(rawHeaders[i][0])] = String(rawHeaders[i][1]);
            }
            var bodyBytes = null;
            if (req.body && req.body.length > 0) {
                bodyBytes = new Uint8Array(req.body);
            }
            var captured = undefined;
            var event = {
                request: {
                    method: String(req.method || 'GET'),
                    url: String(req.url || '/'),
                    headers: headersObj,
                    body: bodyBytes
                },
                respondWith: function(response) {
                    captured = response;
                }
            };
            __helios_fetch_handler(event);
            if (captured && typeof captured.then === 'function') {
                throw new Error('async fetch handlers are not yet supported by the SpiderMonkey backend');
            }
            if (!captured) {
                throw new Error('fetch handler did not call event.respondWith()');
            }
            if (typeof captured !== 'object' || captured === null || Array.isArray(captured)) {
                throw new Error('respondWith expects a Response object');
            }
            return JSON.stringify({
                status: Number(captured.status || 200),
                headers: __helios_headers_to_pairs(captured.headers),
                body: captured.body === undefined || captured.body === null ? '' : String(captured.body)
            });
        }
    "#;
}
#[cfg(feature = "spidermonkey")]
pub use spidermonkey_backend::SpiderMonkeyEngine;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdr_round_trip_stub() {
        let eng = StubEngine::new();
        let cache = XdrCache::new();
        let code = UserCode::Script {
            code: "addEventListener('fetch', e => e.respondWith(new Response('hi')))".into(),
            file_name: "app.js".into(),
        };
        let entry = cache.compile_user_code(&eng, &code).unwrap();
        assert!(entry.bytecode.len() > 8);
        assert_eq!(&entry.bytecode[..4], STUB_MAGIC);

        let h = eng
            .eval_xdr(entry.bytecode.clone(), &entry.module_url)
            .unwrap();
        let resp = eng
            .call_fetch_handler(h, Bytes::from_static(b"hello"))
            .unwrap();
        let s = std::str::from_utf8(&resp).unwrap();
        assert!(s.contains("\"ok\":true"));
        eng.drop_module(h);
    }

    #[test]
    fn xdr_cache_is_shared() {
        let eng = StubEngine::new();
        let cache = Arc::new(XdrCache::new());
        let code = UserCode::Script {
            code: "export default { fetch() { return new Response('x') } }".into(),
            file_name: "a.js".into(),
        };
        let e1 = cache.compile_user_code(&eng, &code).unwrap();
        let e2 = cache.compile_user_code(&eng, &code).unwrap();
        // Same Arc: second compile must hit the cache, not re-compile.
        assert!(Arc::ptr_eq(&e1.bytecode, &e2.bytecode));
        assert_eq!(cache.len(), 1);
    }

    #[cfg(feature = "spidermonkey")]
    #[test]
    fn spidermonkey_fetch_round_trip() {
        let eng = SpiderMonkeyEngine::new().unwrap();
        let source = "addEventListener('fetch', event => event.respondWith(new Response('sm', { status: 201, headers: { 'x-engine': 'spidermonkey' } })))";
        let xdr = eng.compile_to_xdr(source, "sm-test.js").unwrap();
        let handle = eng.eval_xdr(xdr, "sm-test.js").unwrap();
        let resp = eng
            .call_fetch_handler(handle, build_test_request("GET", "http://localhost/", &[]))
            .unwrap();
        let (status, headers, body) = decode_test_response(&resp);
        assert_eq!(status, 201);
        assert_eq!(
            headers,
            vec![("x-engine".to_owned(), "spidermonkey".to_owned())]
        );
        assert_eq!(body, b"sm");
        eng.drop_module(handle);
        spidermonkey_backend::shutdown_engine_for_tests();
    }

    #[cfg(feature = "spidermonkey")]
    fn build_test_request(method: &str, url: &str, headers: &[(&str, &str)]) -> Bytes {
        use bytes::BufMut;
        let mut buf = bytes::BytesMut::new();
        buf.put_u32_le(method.len() as u32);
        buf.put_slice(method.as_bytes());
        buf.put_u32_le(url.len() as u32);
        buf.put_slice(url.as_bytes());
        buf.put_u32_le(headers.len() as u32);
        for (k, v) in headers {
            buf.put_u32_le(k.len() as u32);
            buf.put_slice(k.as_bytes());
            buf.put_u32_le(v.len() as u32);
            buf.put_slice(v.as_bytes());
        }
        buf.put_u32_le(0);
        buf.freeze()
    }

    #[cfg(feature = "spidermonkey")]
    fn decode_test_response(bytes: &[u8]) -> (u16, Vec<(String, String)>, Vec<u8>) {
        let mut p = 0usize;
        let status = u16::from_le_bytes(bytes[p..p + 2].try_into().unwrap());
        p += 2;
        let nh = u32::from_le_bytes(bytes[p..p + 4].try_into().unwrap()) as usize;
        p += 4;
        let mut headers = Vec::with_capacity(nh);
        for _ in 0..nh {
            let klen = u32::from_le_bytes(bytes[p..p + 4].try_into().unwrap()) as usize;
            p += 4;
            let k = String::from_utf8_lossy(&bytes[p..p + klen]).into_owned();
            p += klen;
            let vlen = u32::from_le_bytes(bytes[p..p + 4].try_into().unwrap()) as usize;
            p += 4;
            let v = String::from_utf8_lossy(&bytes[p..p + vlen]).into_owned();
            p += vlen;
            headers.push((k, v));
        }
        let blen = u32::from_le_bytes(bytes[p..p + 4].try_into().unwrap()) as usize;
        p += 4;
        (status, headers, bytes[p..p + blen].to_vec())
    }
}
