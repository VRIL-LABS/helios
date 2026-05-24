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
        self.active.iter().next().map(|e| e.value().as_ref().clone())
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
    //! Real SpiderMonkey XDR pipeline. Bridges to `mozjs::jsapi::JS_*`.
    //!
    //! Wired up via the `runtime` crate (spiderfire) so we re-use its
    //! `Runtime` + `RuntimeBuilder` and don't duplicate root management.
    //!
    //! Only the FFI shape is sketched here — the full integration depends
    //! on the spiderfire fork being patched to expose `EncodeScript` /
    //! `DecodeScript`. See `/.github/copilot-instructions/instructions.md`
    //! Phase 2 for the contract.

    use super::*;
    use std::marker::PhantomData;

    /// Production engine backed by the spiderfire `Runtime`. Each worker
    /// thread owns one of these; the underlying SpiderMonkey JS context
    /// is thread-pinned (per WinterJS convention).
    ///
    /// **`Send` + `Sync` status:**  This struct is currently `Send` and
    /// `Sync` because it is a no-op stub with no interior mutable state.
    /// The `PhantomData<*const ()>` field is kept intentionally so that
    /// auto-`Sync` is suppressed the moment any real SpiderMonkey pointer
    /// or `RefCell` is added.  At that point the explicit
    /// `unsafe impl Sync` below **must be removed** and the dispatcher
    /// restructured to use per-thread engines.
    pub struct SpiderMonkeyEngine {
        // Holds spiderfire `runtime::Runtime` + a `module-handle -> JS root`
        // table guarded by an internal `RefCell` — single-threaded inside.
        _not_sync: PhantomData<*const ()>,
    }

    // SAFETY: Both Send and Sync are safe for the current no-op stub.
    // PhantomData<*const ()> suppresses auto-Sync so these impls are
    // explicit and must be reviewed when real SpiderMonkey state is added.
    // Remove `unsafe impl Sync` once the struct holds thread-pinned state.
    unsafe impl Send for SpiderMonkeyEngine {}
    unsafe impl Sync for SpiderMonkeyEngine {}

    impl std::fmt::Debug for SpiderMonkeyEngine {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("SpiderMonkeyEngine")
        }
    }

    impl SpiderMonkeyEngine {
        pub fn new() -> Result<Self, JsError> {
            // 1. Initialize the global `JSEngineHandle` (see
            //    winterjs-main/src/sm_utils.rs::ENGINE).
            // 2. Build a `Runtime` with `RealmOptions` that enable
            //    Baseline + Ion JIT (this is the breakthrough — JIT is
            //    available because we're running native, not in a WASM
            //    sandbox without PROT_EXEC pages).
            // 3. Install standard WinterCG modules + the helios builtins
            //    (webtransport, etc).
            Ok(Self { _not_sync: PhantomData })
        }
    }

    impl JsEngineBackend for SpiderMonkeyEngine {
        fn eval_module(&self, _source: &str, _module_url: &str) -> Result<ModuleHandle, JsError> {
            Err(JsError::msg("spidermonkey backend not yet wired"))
        }

        fn eval_xdr(&self, _xdr: Arc<[u8]>, _module_url: &str) -> Result<ModuleHandle, JsError> {
            Err(JsError::msg("spidermonkey backend not yet wired"))
        }

        fn call_fetch_handler(&self, _h: ModuleHandle, _b: Bytes) -> Result<Bytes, JsError> {
            Err(JsError::msg("spidermonkey backend not yet wired"))
        }

        fn drain_microtasks(&self, _h: ModuleHandle) -> Result<(), JsError> {
            Ok(())
        }
        fn drop_module(&self, _h: ModuleHandle) {}

        fn compile_to_xdr(&self, _source: &str, _module_url: &str) -> Result<Arc<[u8]>, JsError> {
            Err(JsError::msg("spidermonkey backend not yet wired"))
        }
    }
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
}
