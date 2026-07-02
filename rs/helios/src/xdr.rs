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
use boa_engine::{
    ast::{
        expression::{
            access::{PropertyAccess, PropertyAccessField},
            literal::{LiteralKind, PropertyDefinition},
            operator::assign::{AssignOp, AssignTarget},
            Expression,
        },
        function::FunctionBody,
        property::PropertyName,
        scope::Scope,
        Statement, StatementList, StatementListItem,
    },
    interner::Interner,
    js_string,
    parser::Parser as BoaParser,
    Context as BoaContext, JsObject, JsValue, Source,
};
use bytes::{BufMut, Bytes};
use dashmap::DashMap;
use parking_lot::Mutex;
use serde_json::json;

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

    /// Load source text and its module URL from this entry point.
    pub fn load_source(&self) -> Result<(String, String)> {
        match self {
            UserCode::Script { code, file_name } => {
                Ok((code.clone(), file_name.to_string_lossy().into_owned()))
            }
            UserCode::Module(p) => {
                let src = std::fs::read_to_string(p)
                    .with_context(|| format!("Failed to read {}", p.display()))?;
                Ok((src, format!("file://{}", p.display())))
            }
            UserCode::Directory(p) => {
                // Convention: directory entry point is resolved in this order:
                // index.js → main.js → worker.js. The first file that exists
                // wins; remaining candidates are ignored.
                let candidates = ["index.js", "main.js", "worker.js"];
                let entry = candidates
                    .iter()
                    .map(|n| p.join(n))
                    .find(|p| p.is_file())
                    .with_context(|| format!("No entry point found in {}", p.display()))?;
                let src = std::fs::read_to_string(&entry)
                    .with_context(|| format!("Failed to read entry point {}", entry.display()))?;
                Ok((src, format!("file://{}", entry.display())))
            }
            UserCode::Xdr { .. } => {
                anyhow::bail!("XDR entries do not retain their original source text")
            }
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
    entries: DashMap<String, Arc<XdrEntry>>,
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
            // `entries` stores `Arc<XdrEntry>`, so both the lookup and the
            // potential insertion into `active` are cheap refcount bumps;
            // the only deep clone paid here is the single one needed to
            // hand back an owned `XdrEntry` to the caller. `arc_entry` is
            // moved into `active` (not re-cloned) when the key is vacant.
            let arc_entry = e.value().clone();
            drop(e);
            let active_entry = self
                .active
                .entry(key)
                .or_insert_with(|| arc_entry)
                .clone();
            return Ok((*active_entry).clone());
        }

        let (source, module_url) = match code {
            UserCode::Xdr {
                bytecode,
                module_url,
            } => {
                // Already compiled — re-insert under our key and return.
                let entry = XdrEntry {
                    bytecode: bytecode.clone(),
                    module_url: module_url.clone(),
                };
                let arc_entry = Arc::new(entry);
                self.entries.insert(key.clone(), arc_entry.clone());
                self.active.insert(key, arc_entry.clone());
                return Ok((*arc_entry).clone());
            }
            _ => code.load_source()?,
        };

        let xdr = engine
            .compile_to_xdr(&source, &module_url)
            .map_err(|e| anyhow::anyhow!("XDR compile failed: {e}"))?;
        let entry = XdrEntry {
            bytecode: xdr,
            module_url,
        };
        let arc_entry = Arc::new(entry);
        self.entries.insert(key.clone(), arc_entry.clone());
        self.active.insert(key, arc_entry.clone());
        Ok((*arc_entry).clone())
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
        self.first_active_arc().map(|entry| entry.as_ref().clone())
    }

    /// Return the single active entry as a shared pointer, avoiding per-worker
    /// metadata cloning during warm boot.
    pub fn first_active_arc(&self) -> Option<Arc<XdrEntry>> {
        if self.active.len() != 1 {
            return None;
        }
        self.active.iter().next().map(|e| e.value().clone())
    }

    /// Number of cached compilations.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Minimal valid request wire frame for proactive fetch-handler warmup.
pub fn synthetic_fetch_request_wire() -> Bytes {
    let mut out = bytes::BytesMut::with_capacity(96);
    write_wire_bytes(&mut out, b"GET").expect("static value fits in wire format");
    write_wire_bytes(&mut out, b"http://127.0.0.1/").expect("static value fits in wire format");
    out.put_u32_le(0);
    write_wire_bytes(&mut out, b"").expect("static value fits in wire format");
    write_wire_bytes(&mut out, b"127.0.0.1:0").expect("static value fits in wire format");
    write_wire_bytes(&mut out, b"h1").expect("static value fits in wire format");
    out.freeze()
}

/// Extension trait for backends that expose a standalone XDR compilation
/// step separate from full module evaluation. Implementors that want a
/// custom XDR-only adapter can implement [`XdrCompiler`] and expose it
/// via a wrapper engine.
pub trait XdrCompiler: Send + Sync {
    fn compile(&self, source: &str, module_url: &str) -> Result<Arc<[u8]>, JsError>;
}

// ---------------------------------------------------------------------------
// Boa engine
// ---------------------------------------------------------------------------

/// Real, self-contained JavaScript engine used by the default binary.
///
/// This backend embeds Boa so published HELIOS binaries execute user
/// JavaScript instead of returning the fixed [`StubEngine`] response. The
/// SpiderMonkey backend remains the target for JIT/XDR production builds, but
/// this engine provides authentic handler execution without a SpiderMonkey
/// toolchain.
pub struct BoaEngine {
    next_handle: AtomicU32,
    modules: DashMap<u32, BoaModule>,
    runtime: Mutex<BoaRuntime>,
}

// SAFETY: `HeliosDispatcher` creates one `BoaEngine` per worker thread and
// serializes all access to the non-thread-safe Boa `Context` behind `runtime`.
// Boa-managed values never escape the context or cross the `JsEngineBackend`
// boundary; callers exchange only owned Rust `Bytes`, strings, and module
// handles.
unsafe impl Send for BoaEngine {}
// SAFETY: Shared references cannot concurrently access Boa state because every
// operation locks `runtime`. No raw Boa references or GC handles are exposed.
unsafe impl Sync for BoaEngine {}

struct BoaRuntime {
    context: BoaContext,
    fetch_function: JsObject,
}

#[derive(Clone, Debug)]
struct BoaModule {
    static_response_body: Option<Bytes>,
}

impl std::fmt::Debug for BoaEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoaEngine")
            .field("modules", &self.modules.len())
            .finish()
    }
}

impl BoaEngine {
    pub fn new() -> Result<Self, JsError> {
        let mut context = BoaContext::default();
        context
            .eval(Source::from_bytes(HELIOS_JS_POLYFILL))
            .map_err(|e| JsError::msg(format!("failed to load Helios JS polyfill: {e}")))?;
        let fetch_value = context
            .global_object()
            .get(js_string!("__helios_invoke_fetch_json"), &mut context)
            .map_err(boa_error)?;
        let fetch_function = fetch_value
            .as_object()
            .filter(|o| o.is_callable())
            .ok_or_else(|| {
                JsError::msg("internal error: __helios_invoke_fetch_json is not callable")
            })?;
        Ok(Self {
            next_handle: AtomicU32::new(0),
            modules: DashMap::new(),
            runtime: Mutex::new(BoaRuntime {
                context,
                fetch_function,
            }),
        })
    }

    pub fn eval_script_result(&self, source: &str, _module_url: &str) -> Result<String, JsError> {
        let mut runtime = self.runtime.lock();
        let value = runtime
            .context
            .eval(Source::from_bytes(source))
            .map_err(boa_error)?;
        value
            .to_string(&mut runtime.context)
            .map_err(boa_error)?
            .to_std_string()
            .map_err(|e| JsError::msg(format!("JavaScript result is not valid UTF-16: {e}")))
    }

    fn alloc_handle(&self) -> Result<u32, JsError> {
        self.next_handle
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map(|prev| prev + 1)
            .map_err(|_| JsError::msg("module handle counter overflowed u32"))
    }
}

const BOA_MAGIC: &[u8] = b"HBJS";

impl JsEngineBackend for BoaEngine {
    fn eval_module(&self, source: &str, _module_url: &str) -> Result<ModuleHandle, JsError> {
        let static_response_body = infer_static_response_body(source);
        let mut runtime = self.runtime.lock();
        runtime
            .context
            .eval(Source::from_bytes(source))
            .map_err(boa_error)?;
        runtime.context.run_jobs().map_err(boa_error)?;
        let h = self.alloc_handle()?;
        self.modules.insert(
            h,
            BoaModule {
                static_response_body,
            },
        );
        Ok(ModuleHandle(h))
    }

    fn eval_xdr(&self, xdr: Arc<[u8]>, module_url: &str) -> Result<ModuleHandle, JsError> {
        if xdr.len() < 8 || &xdr[..4] != BOA_MAGIC {
            return Err(JsError::msg("not a HELIOS Boa source blob"));
        }
        let len = u32::from_le_bytes(
            xdr[4..8]
                .try_into()
                .map_err(|_| JsError::msg("invalid Boa source blob length"))?,
        ) as usize;
        if 8 + len > xdr.len() {
            return Err(JsError::msg("truncated Boa source blob"));
        }
        let src = std::str::from_utf8(&xdr[8..8 + len])
            .map_err(|e| JsError::msg(format!("invalid UTF-8 in Boa source blob: {e}")))?;
        self.eval_module(src, module_url)
    }

    fn call_fetch_handler(&self, handle: ModuleHandle, req_bytes: Bytes) -> Result<Bytes, JsError> {
        if !self.modules.contains_key(&handle.0) {
            return Err(JsError::msg(format!("unknown handle {}", handle.0)));
        }

        let req = decode_request_wire(&req_bytes)?;
        let request_json = json!({
            "method": req.method,
            "url": req.url,
            "headers": req.headers,
            "body": String::from_utf8_lossy(&req.body),
            "peer": req.peer,
            "protocol": req.protocol,
        });
        let mut runtime = self.runtime.lock();
        let request_json = JsValue::from(js_string!(request_json.to_string().as_str()));
        let fetch_function = runtime.fetch_function.clone();
        let value = fetch_function
            .call(&JsValue::undefined(), &[request_json], &mut runtime.context)
            .map_err(boa_error)?;
        runtime.context.run_jobs().map_err(boa_error)?;
        let response_json = value
            .to_string(&mut runtime.context)
            .map_err(boa_error)?
            .to_std_string()
            .map_err(|e| JsError::msg(format!("JavaScript response is not valid UTF-16: {e}")))?;
        let response: JsResponse = serde_json::from_str(&response_json)
            .map_err(|e| JsError::msg(format!("invalid JavaScript Response serialization: {e}")))?;
        encode_response_wire(response)
    }

    fn static_response_body(&self, handle: ModuleHandle) -> Option<Bytes> {
        self.modules
            .get(&handle.0)
            .and_then(|module| module.static_response_body.clone())
    }

    fn drain_microtasks(&self, _handle: ModuleHandle) -> Result<(), JsError> {
        self.runtime.lock().context.run_jobs().map_err(boa_error)?;
        Ok(())
    }

    fn drop_module(&self, handle: ModuleHandle) {
        self.modules.remove(&handle.0);
    }

    fn compile_to_xdr(&self, source: &str, _module_url: &str) -> Result<Arc<[u8]>, JsError> {
        let source_len = u32::try_from(source.len())
            .map_err(|_| JsError::msg("JavaScript source exceeds 4 GiB"))?;
        let mut buf = Vec::with_capacity(8 + source.len());
        buf.extend_from_slice(BOA_MAGIC);
        buf.extend_from_slice(&source_len.to_le_bytes());
        buf.extend_from_slice(source.as_bytes());
        Ok(Arc::from(buf))
    }
}

impl XdrCompiler for BoaEngine {
    fn compile(&self, source: &str, module_url: &str) -> Result<Arc<[u8]>, JsError> {
        <Self as JsEngineBackend>::compile_to_xdr(self, source, module_url)
    }
}

fn boa_error(error: boa_engine::JsError) -> JsError {
    JsError::msg(error.to_string())
}

fn infer_static_response_body(source: &str) -> Option<Bytes> {
    let mut interner = Interner::default();
    let mut parser = BoaParser::new(Source::from_bytes(source));
    let script = parser
        .parse_script(&Scope::new_global(), &mut interner)
        .ok()?;
    find_static_response_in_statements(script.statements(), &interner)
}

fn find_static_response_in_statements(
    statements: &StatementList,
    interner: &Interner,
) -> Option<Bytes> {
    statements
        .statements()
        .iter()
        .find_map(|item| static_response_from_statement_item(item, interner))
}

fn static_response_from_statement_item(
    item: &StatementListItem,
    interner: &Interner,
) -> Option<Bytes> {
    let StatementListItem::Statement(stmt) = item else {
        return None;
    };
    let Statement::Expression(expr) = stmt.as_ref() else {
        return None;
    };
    match expr {
        Expression::Call(call) => static_response_from_add_event_listener(call, interner),
        Expression::Assign(assign) if assign.op() == AssignOp::Assign => {
            if is_global_fetch_assignment(assign.lhs(), interner) {
                static_response_from_handler(assign.rhs(), interner)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn static_response_from_add_event_listener(
    call: &boa_engine::ast::expression::Call,
    interner: &Interner,
) -> Option<Bytes> {
    if !is_identifier(call.function(), "addEventListener", interner) || call.args().len() != 2 {
        return None;
    }
    if string_literal(call.args().first()?, interner)? != "fetch" {
        return None;
    }
    static_response_from_handler(call.args().get(1)?, interner)
}

fn is_global_fetch_assignment(target: &AssignTarget, interner: &Interner) -> bool {
    let AssignTarget::Access(access) = target else {
        return false;
    };
    let PropertyAccess::Simple(simple) = access else {
        return false;
    };
    is_identifier(simple.target(), "globalThis", interner)
        && matches!(
            simple.field(),
            PropertyAccessField::Const(field)
                if interned_eq(interner, field.sym(), "fetch")
        )
}

fn static_response_from_handler(expr: &Expression, interner: &Interner) -> Option<Bytes> {
    match expr {
        Expression::ArrowFunction(f) => static_response_from_function_body(f.body(), interner),
        Expression::FunctionExpression(f) => static_response_from_function_body(f.body(), interner),
        _ => None,
    }
}

fn static_response_from_function_body(body: &FunctionBody, interner: &Interner) -> Option<Bytes> {
    if body.statements().len() != 1 {
        return None;
    }
    let StatementListItem::Statement(stmt) = body.statements().first()? else {
        return None;
    };
    match stmt.as_ref() {
        Statement::Return(ret) => static_response_from_handler_result(ret.target()?, interner),
        Statement::Expression(Expression::Call(call)) => {
            static_response_from_respond_with(call, interner)
        }
        Statement::Expression(expr) => static_response_from_handler_result(expr, interner),
        _ => None,
    }
}

fn static_response_from_handler_result(expr: &Expression, interner: &Interner) -> Option<Bytes> {
    static_response_body_from_new_response(expr, interner).or_else(|| {
        let Expression::Call(call) = expr else {
            return None;
        };
        static_response_from_respond_with(call, interner)
    })
}

fn static_response_from_respond_with(
    call: &boa_engine::ast::expression::Call,
    interner: &Interner,
) -> Option<Bytes> {
    let Expression::PropertyAccess(PropertyAccess::Simple(access)) = call.function() else {
        return None;
    };
    if !matches!(
        access.field(),
        PropertyAccessField::Const(field)
            if interned_eq(interner, field.sym(), "respondWith")
    ) || call.args().len() != 1
    {
        return None;
    }
    static_response_body_from_new_response(call.args().first()?, interner)
}

fn static_response_body_from_new_response(expr: &Expression, interner: &Interner) -> Option<Bytes> {
    let Expression::New(new_expr) = expr else {
        return None;
    };
    if !is_identifier(new_expr.constructor(), "Response", interner) {
        return None;
    }
    let args = new_expr.arguments();
    if args.len() > 2 {
        return None;
    }
    if let Some(init) = args.get(1) {
        if !is_default_response_init(init, interner) {
            return None;
        }
    }
    args.first()
        .map(|body| string_literal(body, interner).map(Bytes::from))
        .unwrap_or_else(|| Some(Bytes::new()))
}

fn is_default_response_init(expr: &Expression, interner: &Interner) -> bool {
    let Expression::ObjectLiteral(obj) = expr else {
        return false;
    };
    obj.properties().iter().all(|property| match property {
        PropertyDefinition::Property(PropertyName::Literal(name), value) => {
            match interned_string(interner, name.sym()).as_str() {
                "status" => numeric_literal(value) == Some(200.0),
                "headers" => is_empty_object(value),
                _ => false,
            }
        }
        _ => false,
    })
}

fn is_empty_object(expr: &Expression) -> bool {
    matches!(expr, Expression::ObjectLiteral(obj) if obj.properties().is_empty())
}

fn is_identifier(expr: &Expression, expected: &str, interner: &Interner) -> bool {
    matches!(
        expr, Expression::Identifier(ident) if interned_eq(interner, ident.sym(), expected)
    )
}

fn string_literal(expr: &Expression, interner: &Interner) -> Option<String> {
    let Expression::Literal(lit) = expr else {
        return None;
    };
    match lit.kind() {
        LiteralKind::String(sym) => Some(interned_string(interner, *sym)),
        LiteralKind::Null => Some("null".to_owned()),
        LiteralKind::Bool(value) => Some(value.to_string()),
        LiteralKind::Num(value) => Some(value.to_string()),
        LiteralKind::Int(value) => Some(value.to_string()),
        _ => None,
    }
}

fn numeric_literal(expr: &Expression) -> Option<f64> {
    let Expression::Literal(lit) = expr else {
        return None;
    };
    match lit.kind() {
        LiteralKind::Num(value) => Some(*value),
        LiteralKind::Int(value) => Some(f64::from(*value)),
        _ => None,
    }
}

fn interned_eq(interner: &Interner, sym: boa_engine::interner::Sym, expected: &str) -> bool {
    interner.resolve_expect(sym).to_string() == expected
}

fn interned_string(interner: &Interner, sym: boa_engine::interner::Sym) -> String {
    interner.resolve_expect(sym).to_string()
}

#[derive(Debug)]
struct JsRequest {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    peer: String,
    protocol: String,
}

#[derive(Debug, serde::Deserialize)]
struct JsResponse {
    status: u16,
    #[serde(default)]
    headers: Vec<(String, String)>,
    #[serde(default)]
    body: String,
}

fn decode_request_wire(bytes: &Bytes) -> Result<JsRequest, JsError> {
    let mut p = 0usize;
    let method = read_wire_string(bytes, &mut p)?;
    let url = read_wire_string(bytes, &mut p)?;
    let header_count = read_wire_u32(bytes, &mut p)?;
    // Match the HTTP response decoder's limit to bound allocation and reject
    // request smuggling attempts with pathological header counts.
    if header_count > 256 {
        return Err(JsError::msg("request contains too many headers"));
    }
    let mut headers = Vec::with_capacity(header_count as usize);
    for _ in 0..header_count {
        let name = read_wire_string(bytes, &mut p)?;
        let value = String::from_utf8_lossy(read_wire_bytes(bytes, &mut p)?).into_owned();
        headers.push((name, value));
    }
    let body = read_wire_bytes(bytes, &mut p)?.to_vec();
    let peer = read_wire_string(bytes, &mut p)?;
    let protocol = read_wire_string(bytes, &mut p)?;
    Ok(JsRequest {
        method,
        url,
        headers,
        body,
        peer,
        protocol,
    })
}

fn encode_response_wire(response: JsResponse) -> Result<Bytes, JsError> {
    // Hyper accepts the complete RFC status-code space (100..=599), including
    // uncommon extension codes used by some gateways.
    if !(100..=599).contains(&response.status) {
        return Err(JsError::msg(format!(
            "invalid response status {}",
            response.status
        )));
    }
    let header_count = u32::try_from(response.headers.len())
        .map_err(|_| JsError::msg("response contains too many headers"))?;
    let mut out = bytes::BytesMut::with_capacity(64 + response.body.len());
    out.put_u16_le(response.status);
    out.put_u32_le(header_count);
    for (name, value) in response.headers {
        write_wire_bytes(&mut out, name.as_bytes())?;
        write_wire_bytes(&mut out, value.as_bytes())?;
    }
    write_wire_bytes(&mut out, response.body.as_bytes())?;
    Ok(out.freeze())
}

fn write_wire_bytes(out: &mut bytes::BytesMut, value: &[u8]) -> Result<(), JsError> {
    let len = u32::try_from(value.len()).map_err(|_| JsError::msg("wire value exceeds 4 GiB"))?;
    out.put_u32_le(len);
    out.put_slice(value);
    Ok(())
}

fn read_wire_u32(bytes: &Bytes, p: &mut usize) -> Result<u32, JsError> {
    if *p + 4 > bytes.len() {
        return Err(JsError::msg("truncated request wire data"));
    }
    let value = u32::from_le_bytes(
        bytes[*p..*p + 4]
            .try_into()
            .map_err(|_| JsError::msg("invalid request wire integer"))?,
    );
    *p += 4;
    Ok(value)
}

fn read_wire_bytes<'a>(bytes: &'a Bytes, p: &mut usize) -> Result<&'a [u8], JsError> {
    let len = read_wire_u32(bytes, p)? as usize;
    if *p + len > bytes.len() {
        return Err(JsError::msg("truncated request wire data"));
    }
    let value = &bytes[*p..*p + len];
    *p += len;
    Ok(value)
}

fn read_wire_string(bytes: &Bytes, p: &mut usize) -> Result<String, JsError> {
    String::from_utf8(read_wire_bytes(bytes, p)?.to_vec())
        .map_err(|e| JsError::msg(format!("invalid UTF-8 in request wire data: {e}")))
}

const HELIOS_JS_POLYFILL: &str = r#"
globalThis.__helios_fetch_handler = undefined;

class Headers {
  constructor(init = {}) {
    this.map = {};
    if (Array.isArray(init)) {
      for (const pair of init) this.set(pair[0], pair[1]);
    } else {
      for (const key of Object.keys(init)) this.set(key, init[key]);
    }
  }
  set(name, value) { this.map[String(name).toLowerCase()] = String(value); }
  get(name) {
    const value = this.map[String(name).toLowerCase()];
    return value === undefined ? null : value;
  }
  entries() { return Object.entries(this.map); }
}

class Request {
  constructor(data) {
    this.method = data.method || "GET";
    this.url = data.url || "/";
    this.headers = new Headers(data.headers || []);
    this.body = data.body || "";
  }
  text() { return Promise.resolve(this.body); }
  json() { return Promise.resolve(JSON.parse(this.body)); }
}

class Response {
  constructor(body = "", init = {}) {
    this.body = body == null ? "" : String(body);
    this.status = init.status === undefined ? 200 : Number(init.status);
    this.headers = new Headers(init.headers || {});
  }
  text() { return Promise.resolve(this.body); }
  json() { return Promise.resolve(JSON.parse(this.body)); }
}

globalThis.Headers = Headers;
globalThis.Request = Request;
globalThis.Response = Response;
globalThis.addEventListener = function(type, handler) {
  if (String(type) === "fetch") globalThis.__helios_fetch_handler = handler;
};

globalThis.__helios_invoke_fetch = function(data) {
  const request = new Request(data);
  const event = {
    request,
    response: undefined,
    respondWith(value) { this.response = value; }
  };
  request.respondWith = event.respondWith.bind(event);

  let returned;
  if (typeof globalThis.__helios_fetch_handler === "function") {
    returned = globalThis.__helios_fetch_handler(event);
  } else if (typeof globalThis.fetch === "function") {
    returned = globalThis.fetch(request);
  } else {
    throw new Error("no fetch handler registered");
  }

  let response = event.response !== undefined ? event.response : returned;
  if (response && typeof response.then === "function") {
    throw new Error("Async fetch handlers (Promise-based) are not yet supported by the embedded Boa engine. Use synchronous handlers or enable the SpiderMonkey backend.");
  }
  if (!(response instanceof Response)) {
    response = new Response(response === undefined ? "" : String(response));
  }
  return JSON.stringify({
    status: response.status,
    headers: response.headers.entries(),
    body: response.body
  });
};

globalThis.__helios_invoke_fetch_json = function(data) {
  return globalThis.__helios_invoke_fetch(JSON.parse(data));
};
"#;

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
            Ok(Self {
                _not_sync: PhantomData,
            })
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

    fn encode_test_request(path: &str) -> Bytes {
        let mut out = bytes::BytesMut::new();
        write_wire_bytes(&mut out, b"GET").unwrap();
        write_wire_bytes(&mut out, path.as_bytes()).unwrap();
        out.put_u32_le(0);
        write_wire_bytes(&mut out, b"").unwrap();
        write_wire_bytes(&mut out, b"127.0.0.1:12345").unwrap();
        write_wire_bytes(&mut out, b"h1").unwrap();
        out.freeze()
    }

    #[test]
    fn boa_engine_executes_fetch_handler_code() {
        let eng = BoaEngine::new().unwrap();
        let h = eng
            .eval_module(
                "addEventListener('fetch', event => event.respondWith(new Response('js:' + event.request.url, { status: 201, headers: { 'x-js': 'executed' } })))",
                "app.js",
            )
            .unwrap();
        let resp = eng
            .call_fetch_handler(h, encode_test_request("/hello"))
            .unwrap();

        let mut p = 0usize;
        assert_eq!(read_u16(&resp, &mut p).unwrap(), 201);
        assert_eq!(read_wire_u32(&resp, &mut p).unwrap(), 1);
        assert_eq!(read_wire_string(&resp, &mut p).unwrap(), "x-js");
        assert_eq!(read_wire_string(&resp, &mut p).unwrap(), "executed");
        assert_eq!(read_wire_string(&resp, &mut p).unwrap(), "js:/hello");
    }

    #[test]
    fn synthetic_fetch_request_wire_decodes_and_is_usable() {
        let wire = synthetic_fetch_request_wire();
        let req = decode_request_wire(&wire).expect("synthetic wire must be well-formed");
        assert_eq!(req.method, "GET");
        assert_eq!(req.url, "http://127.0.0.1/");
        assert!(req.headers.is_empty());
        assert!(req.body.is_empty());
        assert_eq!(req.peer, "127.0.0.1:0");
        assert_eq!(req.protocol, "h1");

        // The synthetic frame must also be accepted by a real engine's fetch
        // handler, mirroring how it's used to warm up JIT tiers/caches.
        let eng = BoaEngine::new().unwrap();
        let h = eng
            .eval_module(
                "addEventListener('fetch', event => event.respondWith(new Response('warm')))",
                "warmup.js",
            )
            .unwrap();
        let resp = eng.call_fetch_handler(h, synthetic_fetch_request_wire());
        assert!(resp.is_ok(), "warmup request should be handled: {resp:?}");
    }

    fn read_u16(b: &Bytes, p: &mut usize) -> Option<u16> {
        if *p + 2 > b.len() {
            return None;
        }
        let v = u16::from_le_bytes(b[*p..*p + 2].try_into().ok()?);
        *p += 2;
        Some(v)
    }

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
    fn boa_static_response_body_is_inferred_from_safe_fetch_handler_ast() {
        let eng = BoaEngine::new().unwrap();
        let h = eng
            .eval_module(
                "// new Response('wrong')\naddEventListener('fetch', e => e.respondWith(new Response('hello')))",
                "app.js",
            )
            .unwrap();

        assert_eq!(
            eng.static_response_body(h),
            Some(Bytes::from_static(b"hello"))
        );
    }

    #[test]
    fn boa_static_response_body_rejects_request_dependent_handler() {
        let eng = BoaEngine::new().unwrap();
        let h = eng
            .eval_module(
                "addEventListener('fetch', event => event.respondWith(new Response('js:' + event.request.url)))",
                "app.js",
            )
            .unwrap();

        assert_eq!(eng.static_response_body(h), None);
    }

    #[test]
    fn boa_static_response_body_supports_global_fetch_assignment() {
        let eng = BoaEngine::new().unwrap();
        let h = eng
            .eval_module("globalThis.fetch = () => new Response('main')", "app.js")
            .unwrap();

        assert_eq!(
            eng.static_response_body(h),
            Some(Bytes::from_static(b"main"))
        );
    }

    #[test]
    fn boa_static_response_body_supports_block_bodied_fetch_handler() {
        let eng = BoaEngine::new().unwrap();
        let h = eng
            .eval_module(
                "addEventListener('fetch', event => { event.respondWith(new Response('hello')); })",
                "app.js",
            )
            .unwrap();

        assert_eq!(
            eng.static_response_body(h),
            Some(Bytes::from_static(b"hello"))
        );
    }

    #[test]
    fn directory_entry_point_requires_file_candidate() {
        let unique = format!(
            "helios-xdr-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let dir = std::env::temp_dir().join(unique);
        std::fs::create_dir(&dir).unwrap();
        std::fs::create_dir(dir.join("index.js")).unwrap();
        std::fs::write(
            dir.join("main.js"),
            "globalThis.fetch = () => new Response('main')",
        )
        .unwrap();

        let code = UserCode::Directory(dir.clone());
        let (source, module_url) = code.load_source().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(source, "globalThis.fetch = () => new Response('main')");
        assert!(module_url.ends_with("/main.js"));
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
