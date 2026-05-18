//! Engine abstraction. Phase 2/3 both depend on this trait — the dispatcher
//! is generic over it, so the rest of HELIOS compiles regardless of whether
//! the real SpiderMonkey backend (feature `spidermonkey`) is linked in.

use std::fmt;
use std::sync::Arc;

use bytes::Bytes;

/// Opaque handle to a compiled+evaluated JS module living in the engine.
/// Mirrors the WIT `module-handle = u32`.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct ModuleHandle(pub u32);

/// Error raised by the JS engine. Mirrors the WIT `js-error` record.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct JsError {
    pub message: String,
    pub stack: Option<String>,
    pub location: Option<String>,
}

impl JsError {
    pub fn msg(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
            stack: None,
            location: None,
        }
    }
}

/// The engine backend trait.
///
/// HELIOS provides two implementations:
///
/// * [`crate::xdr::StubEngine`] — pure-Rust stub that simulates module
///   evaluation by storing handlers as Rust closures. Used in tests and
///   when the `spidermonkey` feature is off.
/// * `helios::wit_host::SpiderMonkeyEngine` — the production backend
///   (only built when `spidermonkey` is enabled).
///
/// Implementors must be `Send + Sync`: the dispatcher spreads requests
/// across worker threads, each of which holds its own engine instance,
/// but module handles + XDR blobs are shared across them.
pub trait JsEngineBackend: Send + Sync + fmt::Debug {
    /// Compile + evaluate a module from source, returning a handle.
    fn eval_module(&self, source: &str, module_url: &str) -> Result<ModuleHandle, JsError>;

    /// Decode a precompiled XDR blob and evaluate it. ~10x faster than
    /// `eval_module` because lexing + AST construction are skipped.
    fn eval_xdr(&self, xdr: Arc<[u8]>, module_url: &str) -> Result<ModuleHandle, JsError>;

    /// Invoke the module's registered `fetch` handler.
    fn call_fetch_handler(&self, handle: ModuleHandle, req_bytes: Bytes) -> Result<Bytes, JsError>;

    /// Optional zero-allocation static response for engines that can prove a
    /// handler does not depend on request data. Dynamic JavaScript engines
    /// should keep the default and use [`Self::call_fetch_handler`].
    fn static_response_body(&self, _handle: ModuleHandle) -> Option<Bytes> {
        None
    }

    /// Drain microtasks after the handler resolves.
    fn drain_microtasks(&self, handle: ModuleHandle) -> Result<(), JsError>;

    /// Optional minor-GC hint between requests.
    fn gc_minor(&self, _handle: ModuleHandle) {}

    /// Drop the module and release its SpiderMonkey roots.
    fn drop_module(&self, handle: ModuleHandle);

    /// Compile JS source to SpiderMonkey XDR bytecode (Phase 2). Backends
    /// that don't have a real encoder return an error; the dispatcher will
    /// then fall back to `eval_module` on each worker.
    fn compile_to_xdr(&self, _source: &str, _module_url: &str) -> Result<Arc<[u8]>, JsError> {
        Err(JsError::msg("XDR encoding not supported by this backend"))
    }
}
