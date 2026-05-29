//! Boa-based JavaScript execution engine.
//!
//! Implements [`JsEngineBackend`] using the Boa ECMAScript engine, enabling
//! full JS execution of Workers-style `fetch` event handlers without
//! requiring the SpiderMonkey native toolchain.
//!
//! ## Workers API support
//!
//! This backend implements the minimal Workers `fetch` event surface:
//!
//! * `addEventListener('fetch', handler)` — registers a fetch handler.
//! * `Response(body, init?)` — constructs a response with optional status/headers.
//! * `event.respondWith(response)` — sets the response for the current request.
//! * `Request` — basic request object with `method`, `url`, `headers`, `body` properties.
//!   `body` is a `Uint8Array` to preserve binary payloads faithfully.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use boa_engine::{
    js_string, Context, JsArgs, JsNativeError, JsObject, JsResult, JsString, JsValue,
    NativeFunction, Source,
};
use boa_engine::object::builtins::{JsPromise, JsUint8Array};
use boa_engine::property::PropertyKey;
use bytes::Bytes;
use dashmap::DashMap;

use crate::engine::{JsEngineBackend, JsError, ModuleHandle};

/// Response data captured from JS execution.
#[derive(Clone, Debug)]
struct CapturedResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// Per-module state: the source code and a generation counter used to signal
/// when a module has been dropped (invalidating cached worker contexts).
struct ModuleState {
    source: String,
    generation: u64,
}

/// Per-worker-thread context: a long-lived Boa `Context` with the Workers API and
/// user script already installed, reused across requests from the same thread.
struct WorkerContext {
    context: Context,
    /// The generation at which this context was created; if the engine's generation
    /// for this handle has advanced, this context is stale and must be evicted.
    generation: u64,
    /// Cached fetch handler function object, avoiding a global property lookup per request.
    fetch_handler: JsObject,
}

/// Global generation counter incremented each time a module is dropped.  Worker
/// threads compare this against their cached `WorkerContext::generation` and evict
/// stale entries.
static MODULE_GENERATION: AtomicU64 = AtomicU64::new(0);

/// A set of currently-active handle IDs per engine.  When `drop_module` removes a
/// handle from this set, worker threads will detect the missing entry on their next
/// access and evict the corresponding cached context.
static ACTIVE_HANDLES: std::sync::LazyLock<DashMap<(u64, u32), u64>> =
    std::sync::LazyLock::new(DashMap::new);

thread_local! {
    /// Per-thread cache mapping `(engine_id, handle_id)` to an initialised worker context.
    /// Each dispatcher worker thread owns its own entry, so the script is parsed and
    /// evaluated only once per thread rather than on every request.
    static WORKER_CONTEXTS: RefCell<HashMap<(u64, u32), WorkerContext>> =
        RefCell::new(HashMap::new());

    /// Per-thread slot for capturing the response from `respondWith()`.
    /// The native `respondWith` function writes here; Rust reads it after the handler.
    /// This avoids allocating a new closure or Arc<Mutex> on every request.
    static RESPONSE_SLOT: RefCell<Option<JsValue>> = RefCell::new(None);
}

static NEXT_ENGINE_ID: AtomicU64 = AtomicU64::new(0);

/// Boa-based JS engine that actually executes JavaScript handlers.
///
/// Each call to `eval_module` validates the source by installing the Workers API and
/// evaluating it in a temporary context, surfacing parse/top-level errors before the
/// server starts serving traffic.  `call_fetch_handler` lazily initialises a
/// per-thread [`WorkerContext`] on the first call from a given thread and reuses it
/// for all subsequent requests, avoiding the cost of re-parsing the script per request.
///
/// When a module is dropped via `drop_module`, its entry is removed from the global
/// `ACTIVE_HANDLES` map and the generation counter is bumped.  Worker threads detect
/// stale entries by comparing generations on the next access and evict them, ensuring
/// no unbounded leak of Boa contexts across threads.
///
/// # Thread safety
///
/// `BoaEngine` contains only `u64`, `AtomicU32`, and `DashMap<u32, ModuleState>`
/// where `ModuleState` holds two `String`s.  All of these are `Send + Sync`, so
/// both auto-traits are derived automatically — no manual `unsafe impl` required.
pub struct BoaEngine {
    engine_id: u64,
    next_handle: AtomicU32,
    modules: DashMap<u32, ModuleState>,
}

impl std::fmt::Debug for BoaEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoaEngine")
            .field("engine_id", &self.engine_id)
            .field("modules", &self.modules.len())
            .finish()
    }
}

impl BoaEngine {
    pub fn new() -> Self {
        Self {
            engine_id: NEXT_ENGINE_ID.fetch_add(1, Ordering::Relaxed),
            next_handle: AtomicU32::new(0),
            modules: DashMap::new(),
        }
    }

    fn alloc_handle(&self) -> Result<u32, JsError> {
        self.next_handle
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map(|prev| prev + 1)
            .map_err(|_| JsError::msg("module handle counter overflowed u32"))
    }

    /// Install minimal Workers API: `addEventListener` and the `Response` constructor.
    fn install_workers_api(context: &mut Context) -> JsResult<()> {
        // addEventListener('fetch', handler) — stores handler in a global
        let add_event_listener =
            NativeFunction::from_copy_closure(move |_this, args, ctx| {
                let event_type = args.get_or_undefined(0).to_string(ctx)?;
                if event_type.to_std_string_escaped() == "fetch" {
                    let handler = args.get_or_undefined(1).clone();
                    ctx.global_object().set(
                        js_string!("__helios_fetch_handler"),
                        handler,
                        false,
                        ctx,
                    )?;
                }
                Ok(JsValue::undefined())
            });

        context.global_object().set(
            js_string!("addEventListener"),
            add_event_listener.to_js_function(context.realm()),
            false,
            context,
        )?;

        // __helios_respond_with: a permanent native function that stores the response
        // in the thread-local RESPONSE_SLOT.  Installed once per Context, avoiding
        // per-request closure allocation entirely.
        let respond_with_fn =
            NativeFunction::from_copy_closure(|_this, args, _ctx| {
                let resp_val = args.get_or_undefined(0).clone();
                RESPONSE_SLOT.with(|slot| {
                    *slot.borrow_mut() = Some(resp_val);
                });
                Ok(JsValue::undefined())
            });

        context.global_object().set(
            js_string!("__helios_respond_with"),
            respond_with_fn.to_js_function(context.realm()),
            false,
            context,
        )?;

        // Response constructor: new Response(body, init?)
        // Defined via eval so it acts as a true constructor that works with `instanceof`.
        context.eval(Source::from_bytes(
            br#"
            function Response(body, init) {
                if (!(this instanceof Response)) {
                    return new Response(body, init);
                }
                this.body = (body === undefined || body === null) ? '' : String(body);
                this.__is_response = true;
                this.status = 200;
                this.headers = {};
                if (init && typeof init === 'object') {
                    if (init.status !== undefined) {
                        this.status = Number(init.status);
                    }
                    if (init.headers && typeof init.headers === 'object') {
                        this.headers = init.headers;
                    }
                }
            }
            "#,
        ))
        .map_err(|e| {
            JsNativeError::typ().with_message(format!("failed to define Response: {e}"))
        })?;

        Ok(())
    }

    /// Create a fetch event object with a `respondWith()` method for this request.
    ///
    /// `respondWith` is a lightweight JS function that stores its argument in the
    /// `__helios_response` global.  Rust extracts and decodes the response after the
    /// handler returns, avoiding per-request native closure allocation.
    fn create_fetch_event(
        context: &mut Context,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> JsResult<JsObject> {
        let event = JsObject::with_null_proto();

        // event.request
        let request = JsObject::with_null_proto();
        request.set(js_string!("method"), js_string!(method), false, context)?;
        request.set(js_string!("url"), js_string!(url), false, context)?;

        // Only create headers object if there are headers to set.
        if headers.is_empty() {
            request.set(js_string!("headers"), JsValue::from(JsObject::with_null_proto()), false, context)?;
        } else {
            let headers_obj = JsObject::with_null_proto();
            for (k, v) in headers {
                headers_obj.set(
                    JsString::from(k.as_str()),
                    js_string!(v.as_str()),
                    false,
                    context,
                )?;
            }
            request.set(js_string!("headers"), JsValue::from(headers_obj), false, context)?;
        }

        // Expose body as a Uint8Array so binary payloads are preserved faithfully.
        // For empty bodies (common for GET requests), set null to skip typed-array allocation.
        if body.is_empty() {
            request.set(js_string!("body"), JsValue::null(), false, context)?;
        } else {
            let body_array = JsUint8Array::from_iter(body.iter().copied(), context)
                .map_err(|e| {
                    JsNativeError::typ()
                        .with_message(format!("failed to create body Uint8Array: {e}"))
                })?;
            request.set(js_string!("body"), body_array, false, context)?;
        }

        event.set(js_string!("request"), JsValue::from(request), false, context)?;

        // respondWith: reference the pre-installed __helios_respond_with native
        // function from the context global.  No per-request allocation needed.
        let respond_with_val = context.global_object().get(
            js_string!("__helios_respond_with"),
            context,
        )?;
        event.set(
            js_string!("respondWith"),
            respond_with_val,
            false,
            context,
        )?;

        Ok(event)
    }

    /// Extract the response from the thread-local `RESPONSE_SLOT` after the fetch
    /// handler has run.  Handles Promise values by driving the job queue.
    fn extract_response(context: &mut Context) -> Result<CapturedResponse, JsError> {
        let resp_val = RESPONSE_SLOT.with(|slot| slot.borrow_mut().take())
            .ok_or_else(|| JsError::msg("fetch handler did not call event.respondWith()"))?;

        let resp_obj = resp_val.as_object().ok_or_else(|| {
            JsError::msg("respondWith expects a Response object")
        })?;

        // Detect Promise values: if the argument is a Promise, drive the
        // microtask queue until it settles, then extract the resolved value.
        let resp_obj = match JsPromise::from_object(resp_obj.clone()) {
            Ok(promise) => {
                // Drive the microtask/job queue so the promise can settle.
                context.run_jobs().map_err(|e| {
                    JsError::msg(format!("error running jobs for async handler: {e}"))
                })?;
                match promise.state() {
                    boa_engine::builtins::promise::PromiseState::Fulfilled(val) => {
                        val.as_object().ok_or_else(|| {
                            JsError::msg("async fetch handler resolved with a non-object value")
                        })?.clone()
                    }
                    boa_engine::builtins::promise::PromiseState::Rejected(val) => {
                        return Err(JsError::msg(format!(
                            "async fetch handler rejected: {}",
                        val.display()
                    )));
                }
                boa_engine::builtins::promise::PromiseState::Pending => {
                    return Err(JsError::msg(
                        "async fetch handler returned a Promise that did not settle \
                         synchronously; await-based async handlers are not yet supported",
                    ));
                }
            }
            }
            Err(_) => resp_obj.clone(),
        };

        let status = resp_obj
            .get(js_string!("status"), context)
            .ok()
            .and_then(|v| v.to_number(context).ok())
            .unwrap_or(200.0) as u16;

        let body = resp_obj.get(js_string!("body"), context)
            .map_err(|e| JsError::msg(format!("failed to get response body: {e}")))?;
        let body_str = if body.is_undefined() || body.is_null() {
            String::new()
        } else {
            body.to_string(context)
                .map_err(|e| JsError::msg(format!("failed to convert body to string: {e}")))?
                .to_std_string_escaped()
        };

        let mut headers = Vec::new();
        let headers_val = resp_obj.get(js_string!("headers"), context)
            .map_err(|e| JsError::msg(format!("failed to get response headers: {e}")))?;
        if let Some(h_obj) = headers_val.as_object() {
            // Only collect string-keyed properties; symbol and index keys are
            // not valid HTTP header names and are skipped.
            let keys = h_obj.own_property_keys(context)
                .map_err(|e| JsError::msg(format!("failed to enumerate headers: {e}")))?;
            for key in keys {
                let k_str = match &key {
                    PropertyKey::String(s) => s.to_std_string_escaped(),
                    _ => continue,
                };
                let v = h_obj.get(key, context)
                    .map_err(|e| JsError::msg(format!("failed to get header value: {e}")))?;
                let v_str = v.to_string(context)
                    .map_err(|e| JsError::msg(format!("failed to convert header value: {e}")))?
                    .to_std_string_escaped();
                headers.push((k_str, v_str));
            }
        }

        Ok(CapturedResponse {
            status,
            headers,
            body: body_str.into_bytes(),
        })
    }

    /// Encode a CapturedResponse into the wire format expected by
    /// `decode_response_capnp` in http3.rs.
    fn encode_response(resp: &CapturedResponse) -> Bytes {
        use bytes::BufMut;
        let mut buf =
            bytes::BytesMut::with_capacity(6 + resp.headers.len() * 64 + resp.body.len());
        // status: u16 LE
        buf.put_u16_le(resp.status);
        // header count: u32 LE
        buf.put_u32_le(resp.headers.len() as u32);
        for (k, v) in &resp.headers {
            // header name: u32 len + bytes
            buf.put_u32_le(k.len() as u32);
            buf.put_slice(k.as_bytes());
            // header value: u32 len + bytes
            buf.put_u32_le(v.len() as u32);
            buf.put_slice(v.as_bytes());
        }
        // body: u32 len + bytes
        buf.put_u32_le(resp.body.len() as u32);
        buf.put_slice(&resp.body);
        buf.freeze()
    }

    /// Parse request bytes from the capnp-encoded format used by the dispatcher.
    fn decode_request(req_bytes: &[u8]) -> Option<(String, String, Vec<(String, String)>, Vec<u8>)> {
        let mut p = 0usize;

        let method = read_str(req_bytes, &mut p)?;
        let url = read_str(req_bytes, &mut p)?;

        let nh = read_u32_raw(req_bytes, &mut p)?;
        let mut headers = Vec::with_capacity(nh as usize);
        for _ in 0..nh {
            let k = read_str(req_bytes, &mut p)?;
            let v_bytes = read_raw_bytes(req_bytes, &mut p)?;
            let v = String::from_utf8_lossy(v_bytes).into_owned();
            headers.push((k, v));
        }

        let body = read_raw_bytes(req_bytes, &mut p)?.to_vec();
        Some((method, url, headers, body))
    }
}

fn read_u32_raw(b: &[u8], p: &mut usize) -> Option<u32> {
    if *p + 4 > b.len() {
        return None;
    }
    let v = u32::from_le_bytes(b[*p..*p + 4].try_into().ok()?);
    *p += 4;
    Some(v)
}

fn read_raw_bytes<'a>(b: &'a [u8], p: &mut usize) -> Option<&'a [u8]> {
    let n = read_u32_raw(b, p)? as usize;
    if *p + n > b.len() {
        return None;
    }
    let s = &b[*p..*p + n];
    *p += n;
    Some(s)
}

fn read_str(b: &[u8], p: &mut usize) -> Option<String> {
    let bytes = read_raw_bytes(b, p)?;
    Some(String::from_utf8_lossy(bytes).into_owned())
}

impl JsEngineBackend for BoaEngine {
    fn eval_module(&self, source: &str, _module_url: &str) -> Result<ModuleHandle, JsError> {
        // Validate at startup: install the Workers API and evaluate the actual source so
        // that parse errors and top-level exceptions surface before the server starts
        // accepting requests (fail-fast property of the eval_module entry point).
        let mut context = Context::default();
        Self::install_workers_api(&mut context)
            .map_err(|e| JsError::msg(format!("Workers API install failed: {e}")))?;
        context
            .eval(Source::from_bytes(source.as_bytes()))
            .map_err(|e| JsError::msg(format!("script evaluation failed: {e}")))?;

        let h = self.alloc_handle()?;
        let gen = MODULE_GENERATION.load(Ordering::Acquire);
        self.modules.insert(
            h,
            ModuleState {
                source: source.to_string(),
                generation: gen,
            },
        );
        ACTIVE_HANDLES.insert((self.engine_id, h), gen);
        Ok(ModuleHandle(h))
    }

    fn eval_xdr(&self, xdr: Arc<[u8]>, module_url: &str) -> Result<ModuleHandle, JsError> {
        // For Boa, XDR is just the stub format: magic + len + source
        if xdr.len() < 8 || &xdr[..4] != b"HXDR" {
            return Err(JsError::msg("not a valid XDR blob"));
        }
        let len = u32::from_le_bytes(xdr[4..8].try_into().unwrap()) as usize;
        if 8 + len > xdr.len() {
            return Err(JsError::msg("truncated XDR blob"));
        }
        let src = std::str::from_utf8(&xdr[8..8 + len])
            .map_err(|e| JsError::msg(format!("invalid UTF-8 in XDR: {e}")))?;
        self.eval_module(src, module_url)
    }

    fn call_fetch_handler(
        &self,
        handle: ModuleHandle,
        req_bytes: Bytes,
    ) -> Result<Bytes, JsError> {
        let handle_id = handle.0;
        let (method, url, headers, body) =
            Self::decode_request(&req_bytes).unwrap_or_else(|| {
                ("GET".to_string(), "/".to_string(), vec![], vec![])
            });

        let engine_id = self.engine_id;

        let resp = WORKER_CONTEXTS.with(|cache| -> Result<CapturedResponse, JsError> {
            let mut cache = cache.borrow_mut();
            let key = (engine_id, handle_id);

            // Evict stale entries: only consult the global registry when the global
            // generation counter has advanced since this context was built (fast path:
            // a single relaxed atomic load that almost always matches).
            if let Some(wctx) = cache.get(&key) {
                let current_gen = MODULE_GENERATION.load(Ordering::Relaxed);
                if current_gen != wctx.generation {
                    // Generation changed; use Acquire to synchronize with the Release
                    // in drop_module before consulting ACTIVE_HANDLES.
                    let _synced_gen = MODULE_GENERATION.load(Ordering::Acquire);
                    let still_active = ACTIVE_HANDLES
                        .get(&key)
                        .map(|g| *g == wctx.generation)
                        .unwrap_or(false);
                    if !still_active {
                        cache.remove(&key);
                    }
                }
            }

            // Lazy-initialise the per-thread worker context for this handle.
            // The Context is built once per thread and reused across all subsequent
            // requests, avoiding a full parse+eval cycle on every call.
            if !cache.contains_key(&key) {
                let (source, gen) = {
                    let module = self.modules.get(&handle_id)
                        .ok_or_else(|| JsError::msg(format!("unknown handle {}", handle_id)))?;
                    (module.source.clone(), module.generation)
                };

                let mut context = Context::default();

                Self::install_workers_api(&mut context)
                    .map_err(|e| JsError::msg(format!("failed to install Workers API: {e}")))?;

                context
                    .eval(Source::from_bytes(source.as_bytes()))
                    .map_err(|e| JsError::msg(format!("script evaluation failed: {e}")))?;

                // Verify that the script registered a fetch handler.
                let handler = context
                    .global_object()
                    .get(js_string!("__helios_fetch_handler"), &mut context)
                    .map_err(|e| JsError::msg(format!("failed to get fetch handler: {e}")))?;

                if handler.is_undefined() || handler.is_null() {
                    return Err(JsError::msg(
                        "no fetch handler registered (call addEventListener('fetch', handler))",
                    ));
                }

                let fetch_handler = handler.as_object().ok_or_else(|| {
                    JsError::msg("fetch handler is not an object")
                })?.clone();

                cache.insert(key, WorkerContext { context, generation: gen, fetch_handler });
            }

            let wctx = cache.get_mut(&key).unwrap();

            // Use the cached fetch handler directly — no global property lookup needed.
            let handler_fn = &wctx.fetch_handler;

            // Build a fresh event object for this request.  Creating new JsObjects is
            // cheap compared to re-parsing the script; the GC reclaims them after the call.
            let event = Self::create_fetch_event(
                &mut wctx.context,
                &method,
                &url,
                &headers,
                &body,
            )
            .map_err(|e| JsError::msg(format!("failed to create fetch event: {e}")))?;

            handler_fn
                .call(&JsValue::undefined(), &[event.into()], &mut wctx.context)
                .map_err(|e| JsError::msg(format!("fetch handler threw: {e}")))?;

            // Extract response from the __helios_response global.
            Self::extract_response(&mut wctx.context)
        })?;

        Ok(Self::encode_response(&resp))
    }

    fn drain_microtasks(&self, _handle: ModuleHandle) -> Result<(), JsError> {
        Ok(())
    }

    fn drop_module(&self, handle: ModuleHandle) {
        self.modules.remove(&handle.0);
        let key = (self.engine_id, handle.0);
        // Remove from the global active-handles registry and bump the generation
        // counter.  Worker threads will detect the stale entry on their next access
        // and evict it, preventing unbounded context leaks across threads.
        ACTIVE_HANDLES.remove(&key);
        MODULE_GENERATION.fetch_add(1, Ordering::Release);
        // Eagerly reclaim the cached context on the current thread.
        WORKER_CONTEXTS.with(|cache| {
            cache.borrow_mut().remove(&key);
        });
    }

    fn compile_to_xdr(&self, source: &str, _module_url: &str) -> Result<Arc<[u8]>, JsError> {
        // Use the same stub XDR format for compatibility with the cache
        let mut buf = Vec::with_capacity(8 + source.len());
        buf.extend_from_slice(b"HXDR");
        buf.extend_from_slice(&(source.len() as u32).to_le_bytes());
        buf.extend_from_slice(source.as_bytes());
        Ok(Arc::from(buf))
    }
}

impl crate::xdr::XdrCompiler for BoaEngine {
    fn compile(&self, source: &str, module_url: &str) -> Result<Arc<[u8]>, JsError> {
        <Self as JsEngineBackend>::compile_to_xdr(self, source, module_url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_fetch_handler() {
        let engine = BoaEngine::new();
        let source = r#"addEventListener('fetch', (event) => {
            event.respondWith(new Response('Hello from Helios!'));
        });"#;
        let handle = engine.eval_module(source, "test.js").unwrap();

        // Build a minimal request in the capnp format
        let req_bytes = build_test_request("GET", "http://localhost:8080/", &[], b"");
        let resp_bytes = engine.call_fetch_handler(handle, req_bytes).unwrap();

        // Decode the response
        let resp = decode_test_response(&resp_bytes);
        assert_eq!(resp.0, 200);
        assert_eq!(resp.2, b"Hello from Helios!");
    }

    #[test]
    fn response_with_status() {
        let engine = BoaEngine::new();
        let source = r#"addEventListener('fetch', (event) => {
            event.respondWith(new Response('Not Found', { status: 404 }));
        });"#;
        let handle = engine.eval_module(source, "test.js").unwrap();

        let req_bytes = build_test_request("GET", "http://localhost:8080/missing", &[], b"");
        let resp_bytes = engine.call_fetch_handler(handle, req_bytes).unwrap();

        let resp = decode_test_response(&resp_bytes);
        assert_eq!(resp.0, 404);
        assert_eq!(resp.2, b"Not Found");
    }

    fn build_test_request(
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Bytes {
        use bytes::BufMut;
        let mut buf = bytes::BytesMut::with_capacity(256);
        // method
        buf.put_u32_le(method.len() as u32);
        buf.put_slice(method.as_bytes());
        // url
        buf.put_u32_le(url.len() as u32);
        buf.put_slice(url.as_bytes());
        // headers
        buf.put_u32_le(headers.len() as u32);
        for (k, v) in headers {
            buf.put_u32_le(k.len() as u32);
            buf.put_slice(k.as_bytes());
            buf.put_u32_le(v.len() as u32);
            buf.put_slice(v.as_bytes());
        }
        // body
        buf.put_u32_le(body.len() as u32);
        buf.put_slice(body);
        // peer addr
        buf.put_u32_le(15);
        buf.put_slice(b"127.0.0.1:12345");
        // protocol
        buf.put_u32_le(2);
        buf.put_slice(b"h1");
        buf.freeze()
    }

    fn decode_test_response(bytes: &[u8]) -> (u16, Vec<(String, String)>, Vec<u8>) {
        let mut p = 0usize;
        let status = u16::from_le_bytes(bytes[p..p + 2].try_into().unwrap());
        p += 2;
        let nh = u32::from_le_bytes(bytes[p..p + 4].try_into().unwrap()) as usize;
        p += 4;
        let mut headers = Vec::new();
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
        let body = bytes[p..p + blen].to_vec();
        (status, headers, body)
    }
}
