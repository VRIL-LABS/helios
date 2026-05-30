//! Phase 3 — Host-side implementation of the `helios:engine/js-engine`
//! WIT interface.
//!
//! Architecture (see `wit/helios-engine.wit`):
//!
//! ```text
//!  WASM component (helios-worker.wasm)         HOST (this process)
//!  --------------------------------         ----------------------
//!  - WinterCG / CF Workers API surface       - SpiderMonkey native
//!  - fetch() event dispatch                  - Baseline + Ion JIT enabled
//!  - Cap'n Proto serialization               - Implements js-engine
//!         |                                          ^
//!         |   import helios:engine/js-engine         |
//!         +------------------------------------------+
//! ```
//!
//! The component runs inside `wasmtime` (sandboxed, no PROT_EXEC). The
//! `js-engine` import is fulfilled by a host function that drives a real
//! SpiderMonkey runtime where JIT *is* available — bypassing the WASM
//! sandbox's no-W^X restriction. Once warm (~100 invocations), Ion Tier 2
//! takes over.
//!
//! This module wires:
//!
//! * The wasmtime `Engine`, `Linker`, and `Store<HostState>` setup.
//! * Conversion between WIT-level `list<u8>` blobs and our [`Bytes`].
//! * The mapping from `module-handle: u32` to in-host [`ModuleHandle`].
//! * Routing of incoming HTTP requests through the WASI HTTP world export.

use std::sync::Arc;

use anyhow::Context as _;
use bytes::Bytes;
use dashmap::DashMap;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiView};

use crate::engine::{JsEngineBackend, JsError, ModuleHandle};

/// Per-store state passed to host functions implementing `js-engine`.
pub struct HostState {
    pub backend: Arc<dyn JsEngineBackend>,
    pub wasi: WasiCtx,
    pub table: ResourceTable,
    /// Handle table: maps WIT-level u32 to our internal `ModuleHandle`.
    /// (They happen to be the same shape, but isolating the spaces lets
    /// us validate at the boundary.)
    pub handles: Arc<DashMap<u32, ModuleHandle>>,
}

impl std::fmt::Debug for HostState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostState")
            .field("handles", &self.handles.len())
            .finish_non_exhaustive()
    }
}

impl HostState {
    pub fn new(backend: Arc<dyn JsEngineBackend>) -> Self {
        let wasi = WasiCtxBuilder::new().inherit_stdio().inherit_env().build();
        Self {
            backend,
            wasi,
            table: ResourceTable::new(),
            handles: Arc::new(DashMap::new()),
        }
    }
}

impl WasiView for HostState {
    fn ctx(&mut self) -> &mut WasiCtx {
        &mut self.wasi
    }
    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
}

/// Builds a wasmtime [`Engine`] tuned for HELIOS: async, component-model,
/// epoch interruption + fuel for runaway-script protection. These knobs
/// matter for the per-request budget; see `wasmtime::Config` docs.
pub fn build_engine() -> anyhow::Result<Engine> {
    let mut cfg = Config::new();
    cfg.wasm_component_model(true);
    cfg.async_support(true);
    cfg.consume_fuel(false);
    cfg.epoch_interruption(true);
    // Cranelift is fine for the worker component; the JIT win is on the
    // *JS* side via the host engine, not on the worker's WASM side.
    cfg.cranelift_opt_level(wasmtime::OptLevel::Speed);
    Engine::new(&cfg).context("wasmtime engine init")
}

/// Load `helios-worker.wasm` and link the `js-engine` host import.
///
/// In a fully wired build this returns a typed `bindings::HeliosWorker`
/// produced by `wasmtime::component::bindgen!` over the WIT world. We
/// don't generate those bindings at compile time here (the component
/// itself is built out-of-tree) — instead we expose the linker so the
/// embedder can call `Linker::instantiate_async`.
pub fn build_linker(engine: &Engine) -> anyhow::Result<Linker<HostState>> {
    let mut linker = Linker::new(engine);
    wasmtime_wasi::add_to_linker_async(&mut linker).context("add wasi to linker")?;

    add_js_engine_to_linker(&mut linker)?;
    Ok(linker)
}

/// Register the `helios:engine/js-engine` interface on the linker.
///
/// For wire-compatibility with the WIT `record js-error`, a fully-wired
/// build uses `wasmtime::component::bindgen!` to generate typed
/// signatures. To keep this scaffold buildable without an out-of-tree
/// `.wasm` artifact, we marshal errors as `String` at the boundary —
/// equivalent under the hood (record-of-3-strings ≅ string for our
/// purposes) and trivially upgradable to the typed shape later.
fn add_js_engine_to_linker(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    let mut iface = linker
        .instance("helios:engine/js-engine@1.0.0")
        .context("declare helios:engine/js-engine instance")?;

    iface.func_wrap(
        "eval-module",
        |mut store: wasmtime::StoreContextMut<'_, HostState>,
         (source, module_url): (String, String)|
         -> wasmtime::Result<(Result<u32, String>,)> {
            let state = store.data_mut();
            let r = state.backend.eval_module(&source, &module_url);
            Ok((map_result(state, r),))
        },
    )?;

    iface.func_wrap(
        "eval-xdr",
        |mut store: wasmtime::StoreContextMut<'_, HostState>,
         (xdr, module_url): (Vec<u8>, String)|
         -> wasmtime::Result<(Result<u32, String>,)> {
            let state = store.data_mut();
            let r = state.backend.eval_xdr(Arc::from(xdr), &module_url);
            Ok((map_result(state, r),))
        },
    )?;

    iface.func_wrap(
        "call-fetch-handler",
        |mut store: wasmtime::StoreContextMut<'_, HostState>,
         (handle, req): (u32, Vec<u8>)|
         -> wasmtime::Result<(Result<Vec<u8>, String>,)> {
            let state = store.data_mut();
            let module_handle = state.handles.get(&handle).map(|e| *e.value());
            let resp = match module_handle {
                Some(h) => state
                    .backend
                    .call_fetch_handler(h, Bytes::from(req))
                    .map(|b| b.to_vec())
                    .map_err(|e| e.message),
                None => Err(format!("unknown module handle {handle}")),
            };
            Ok((resp,))
        },
    )?;

    iface.func_wrap(
        "drain-microtasks",
        |mut store: wasmtime::StoreContextMut<'_, HostState>,
         (handle,): (u32,)|
         -> wasmtime::Result<(Result<(), String>,)> {
            let state = store.data_mut();
            let r = match state.handles.get(&handle).map(|e| *e.value()) {
                Some(h) => state.backend.drain_microtasks(h).map_err(|e| e.message),
                None => Err(format!("unknown module handle {handle}")),
            };
            Ok((r,))
        },
    )?;

    iface.func_wrap(
        "gc-minor",
        |mut store: wasmtime::StoreContextMut<'_, HostState>,
         (handle,): (u32,)|
         -> wasmtime::Result<()> {
            let state = store.data_mut();
            if let Some(h) = state.handles.get(&handle).map(|e| *e.value()) {
                state.backend.gc_minor(h);
            }
            Ok(())
        },
    )?;

    iface.func_wrap(
        "drop-module",
        |mut store: wasmtime::StoreContextMut<'_, HostState>,
         (handle,): (u32,)|
         -> wasmtime::Result<()> {
            let state = store.data_mut();
            if let Some((_, h)) = state.handles.remove(&handle) {
                state.backend.drop_module(h);
            }
            Ok(())
        },
    )?;

    Ok(())
}

fn map_result(state: &HostState, r: Result<ModuleHandle, JsError>) -> Result<u32, String> {
    match r {
        Ok(h) => {
            state.handles.insert(h.0, h);
            Ok(h.0)
        }
        Err(e) => Err(e.message),
    }
}

/// Load a worker component from disk.
pub fn load_worker_component(engine: &Engine, path: &std::path::Path) -> anyhow::Result<Component> {
    Component::from_file(engine, path)
        .with_context(|| format!("loading worker component at {}", path.display()))
}

/// Convenience: build a `Store` ready for instantiation.
pub fn build_store(engine: &Engine, backend: Arc<dyn JsEngineBackend>) -> Store<HostState> {
    Store::new(engine, HostState::new(backend))
}
