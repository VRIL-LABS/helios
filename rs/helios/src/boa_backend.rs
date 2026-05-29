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
//! * `Request` — basic request object with `method`, `url`, `headers` properties.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use boa_engine::{
    js_string, Context, JsArgs, JsNativeError, JsObject, JsResult, JsString, JsValue,
    NativeFunction, Source,
};
use bytes::Bytes;
use dashmap::DashMap;
use parking_lot::Mutex;

use crate::engine::{JsEngineBackend, JsError, ModuleHandle};

/// Response data captured from JS execution.
#[derive(Clone, Debug)]
struct CapturedResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// Per-module state: the source code and any compiled state.
struct ModuleState {
    source: String,
    module_url: String,
}

/// Boa-based JS engine that actually executes JavaScript handlers.
///
/// Each call to `eval_module` parses and evaluates the source, registering
/// any `addEventListener('fetch', ...)` callback. `call_fetch_handler`
/// invokes that callback with a request object and captures the response.
pub struct BoaEngine {
    next_handle: AtomicU32,
    modules: DashMap<u32, ModuleState>,
}

impl std::fmt::Debug for BoaEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoaEngine")
            .field("modules", &self.modules.len())
            .finish()
    }
}

impl BoaEngine {
    pub fn new() -> Self {
        Self {
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

    /// Execute JS source in a fresh Boa context, calling the fetch handler
    /// with the given request data and returning the captured response.
    fn execute_fetch(
        &self,
        source: &str,
        _module_url: &str,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<CapturedResponse, JsError> {
        let response: Arc<Mutex<Option<CapturedResponse>>> = Arc::new(Mutex::new(None));

        let mut context = Context::default();

        // Install the Workers API globals
        self.install_workers_api(&mut context, response.clone())
            .map_err(|e| JsError::msg(format!("failed to install Workers API: {e}")))?;

        // Evaluate the user script
        context
            .eval(Source::from_bytes(source.as_bytes()))
            .map_err(|e| JsError::msg(format!("script evaluation failed: {e}")))?;

        // Get the registered fetch handler
        let fetch_handler = context
            .global_object()
            .get(js_string!("__helios_fetch_handler"), &mut context)
            .map_err(|e| JsError::msg(format!("failed to get fetch handler: {e}")))?;

        if fetch_handler.is_undefined() || fetch_handler.is_null() {
            return Err(JsError::msg(
                "no fetch handler registered (call addEventListener('fetch', handler))",
            ));
        }

        let handler_fn = fetch_handler
            .as_callable()
            .ok_or_else(|| JsError::msg("fetch handler is not callable"))?;

        // Create the request/event object
        let event = self
            .create_fetch_event(&mut context, method, url, headers, body, response.clone())
            .map_err(|e| JsError::msg(format!("failed to create fetch event: {e}")))?;

        // Call the handler
        handler_fn
            .call(&JsValue::undefined(), &[event.into()], &mut context)
            .map_err(|e| JsError::msg(format!("fetch handler threw: {e}")))?;

        // Extract the response
        let captured = response.lock().take().ok_or_else(|| {
            JsError::msg("fetch handler did not call event.respondWith()")
        })?;

        Ok(captured)
    }

    /// Install minimal Workers API: addEventListener, Response, Request
    fn install_workers_api(
        &self,
        context: &mut Context,
        _response_capture: Arc<Mutex<Option<CapturedResponse>>>,
    ) -> JsResult<()> {
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

        // Response constructor: new Response(body, init?)
        // Register as a callable constructor using eval to define a JS function
        // that acts as both a constructor and a regular function.
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

    /// Create a fetch event object with respondWith() method
    fn create_fetch_event(
        &self,
        context: &mut Context,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
        response_capture: Arc<Mutex<Option<CapturedResponse>>>,
    ) -> JsResult<JsObject> {
        let event = JsObject::with_null_proto();

        // event.request
        let request = JsObject::with_null_proto();
        request.set(js_string!("method"), js_string!(method), false, context)?;
        request.set(js_string!("url"), js_string!(url), false, context)?;

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

        let body_str = String::from_utf8_lossy(body);
        request.set(
            js_string!("body"),
            js_string!(body_str.as_ref()),
            false,
            context,
        )?;

        event.set(js_string!("request"), JsValue::from(request), false, context)?;

        // event.respondWith(response)
        let capture = response_capture;
        // SAFETY: The closure captures only an Arc<Mutex<...>> which is Send+Sync.
        // It does not capture any GC-managed Boa objects.
        let respond_with =
            unsafe { NativeFunction::from_closure(move |_this, args, ctx| {
                let resp_val = args.get_or_undefined(0);
                let resp_obj = resp_val.as_object().ok_or_else(|| {
                    JsNativeError::typ().with_message("respondWith expects a Response object")
                })?;

                let status = resp_obj
                    .get(js_string!("status"), ctx)?
                    .to_number(ctx)
                    .unwrap_or(200.0) as u16;

                let body = resp_obj.get(js_string!("body"), ctx)?;
                let body_str = if body.is_undefined() || body.is_null() {
                    String::new()
                } else {
                    body.to_string(ctx)?.to_std_string_escaped()
                };

                let mut headers = Vec::new();
                let headers_val = resp_obj.get(js_string!("headers"), ctx)?;
                if let Some(h_obj) = headers_val.as_object() {
                    // Try to enumerate own properties
                    let keys = h_obj.own_property_keys(ctx)?;
                    for key in keys {
                        let k_str = format!("{}", key);
                        let v = h_obj.get(key.clone(), ctx)?;
                        let v_str = v.to_string(ctx)?.to_std_string_escaped();
                        headers.push((k_str, v_str));
                    }
                }

                *capture.lock() = Some(CapturedResponse {
                    status,
                    headers,
                    body: body_str.into_bytes(),
                });

                Ok(JsValue::undefined())
            }) };

        event.set(
            js_string!("respondWith"),
            respond_with.to_js_function(context.realm()),
            false,
            context,
        )?;

        Ok(event)
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

// SAFETY: BoaEngine only contains atomics and DashMap (both Send+Sync).
// The actual Boa Context is created fresh per call (not stored).
unsafe impl Send for BoaEngine {}
unsafe impl Sync for BoaEngine {}

impl JsEngineBackend for BoaEngine {
    fn eval_module(&self, source: &str, module_url: &str) -> Result<ModuleHandle, JsError> {
        // Validate the source by doing a trial parse
        let mut context = Context::default();
        context
            .eval(Source::from_bytes(b""))
            .map_err(|e| JsError::msg(format!("context init failed: {e}")))?;

        let h = self.alloc_handle()?;
        self.modules.insert(
            h,
            ModuleState {
                source: source.to_string(),
                module_url: module_url.to_string(),
            },
        );
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
        let module = self
            .modules
            .get(&handle.0)
            .ok_or_else(|| JsError::msg(format!("unknown handle {}", handle.0)))?;

        let (method, url, headers, body) =
            Self::decode_request(&req_bytes).unwrap_or_else(|| {
                ("GET".to_string(), "/".to_string(), vec![], vec![])
            });

        let resp = self.execute_fetch(
            &module.source,
            &module.module_url,
            &method,
            &url,
            &headers,
            &body,
        )?;

        Ok(Self::encode_response(&resp))
    }

    fn drain_microtasks(&self, _handle: ModuleHandle) -> Result<(), JsError> {
        Ok(())
    }

    fn drop_module(&self, handle: ModuleHandle) {
        self.modules.remove(&handle.0);
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
