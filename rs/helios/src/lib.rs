//! HELIOS — JIT at the edge. Finally.
//!
//! This crate implements the five-phase HELIOS architecture from
//! `.github/copilot-instructions/instructions.md`:
//!
//! * [`dispatcher`] — **Phase 1**: lock-free per-worker dispatch
//!   (`HeliosDispatcher`) replacing `Arc<Mutex<SingleRunner>>`.
//! * [`xdr`] — **Phase 2**: SpiderMonkey XDR bytecode pre-compile pipeline,
//!   shared across all workers via `Arc<[u8]>`.
//! * [`wit_host`] — **Phase 3**: native host implementation of the
//!   `helios:engine/js-engine` WIT interface that places SpiderMonkey JIT
//!   outside the WASM sandbox.
//! * [`http3`] — **Phase 4**: dual-stack hyper-1 / quinn HTTP-3 server with
//!   Alt-Svc advertisement and WebTransport CONNECT upgrades.
//! * [`wizer_build`] — **Phase 5**: `helios build` pipeline that runs
//!   Wizer over the worker module to pre-init SpiderMonkey state.
//!
//! The SpiderMonkey backend itself is gated behind the `spidermonkey` cargo
//! feature; when disabled, [`JsEngineBackend`] is implemented by
//! [`xdr::StubEngine`] so the whole crate still compiles and tests end-to-end.
//!
//! [`JsEngineBackend`]: crate::engine::JsEngineBackend

#![forbid(unsafe_op_in_unsafe_fn)]
#![warn(missing_debug_implementations)]

pub mod bench;
pub mod dispatcher;
pub mod engine;
pub(crate) mod http1_utils;
pub mod http3;
pub mod webtransport;
pub mod wit_host;
pub mod wizer_build;
pub mod xdr;

/// Re-exports of the most-used HELIOS types for downstream embedders.
pub mod prelude {
    pub use crate::dispatcher::{
        DispatchPolicy, HeliosDispatcher, RequestData, ResponseData, WorkerHandle,
    };
    pub use crate::engine::{JsEngineBackend, ModuleHandle};
    pub use crate::xdr::{UserCode, XdrCache};
}
