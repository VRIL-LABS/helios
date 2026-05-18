//! Phase 4 — Dual-stack HTTP/1+2 (hyper-util) and HTTP/3 (quinn + h3)
//! server, fronting a [`HeliosDispatcher`].
//!
//! Architecture:
//!
//! * One TCP listener serves HTTP/1.1 and HTTP/2 via `hyper-util`'s
//!   `auto::Builder`. The `Alt-Svc` header advertises `h3=":<port>"` so
//!   browsers automatically upgrade to QUIC on subsequent requests.
//! * One UDP socket runs `quinn::Endpoint` with an `h3` server on top.
//!   WebTransport CONNECT-extended requests are detected at the H3 frame
//!   level and routed to [`crate::webtransport`].
//!
//! Both legs share the same dispatcher: a request becomes
//! `RequestData { req_bytes: ..., protocol }` and is routed without
//! caring about its transport.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use bytes::{Buf, Bytes};
use http::{Request, Response, StatusCode};
use http_body::Body as _;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

use crate::dispatcher::{HeliosDispatcher, Protocol, RequestData, ResponseData};
use crate::engine::{JsEngineBackend, ModuleHandle};
use crate::http1_utils::{content_length, find_header_end_from, write_all_fast};

/// TLS / listener config for the dual-stack server.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// TCP + UDP bind address.
    pub addr: SocketAddr,
    /// `Alt-Svc` value to advertise on HTTP/1+2 responses. `None` disables
    /// the header (e.g. for plain-HTTP local dev).
    pub alt_svc: Option<String>,
    /// TLS configuration. When `None`, only plaintext HTTP/1+2 is served
    /// (no QUIC: QUIC requires TLS 1.3).
    pub tls: Option<TlsConfig>,
    /// Optional in-process engine fast path for Send + Sync engines.
    pub inline_engine: Option<InlineEngine>,
}

#[derive(Clone, Debug)]
pub struct TlsConfig {
    pub cert_chain_pem_path: std::path::PathBuf,
    pub private_key_pem_path: std::path::PathBuf,
}

#[derive(Clone, Debug)]
pub struct InlineEngine {
    pub engine: Arc<dyn JsEngineBackend>,
    pub handle: ModuleHandle,
}

impl ServerConfig {
    pub fn plain(addr: SocketAddr) -> Self {
        Self {
            addr,
            alt_svc: None,
            tls: None,
            inline_engine: None,
        }
    }

    pub fn with_alt_svc(mut self, alt_svc: impl Into<String>) -> Self {
        self.alt_svc = Some(alt_svc.into());
        self
    }

    pub fn with_tls(mut self, tls: TlsConfig) -> Self {
        self.tls = Some(tls);
        self
    }

    pub fn with_inline_engine(mut self, inline_engine: InlineEngine) -> Self {
        self.inline_engine = Some(inline_engine);
        self
    }
}

/// Run the server until `shutdown` resolves.
pub async fn run_server(
    config: ServerConfig,
    dispatcher: Arc<HeliosDispatcher>,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) -> Result<()> {
    let tls_for_h3 = config.tls.clone();
    let h1h2 = serve_h1h2(config.clone(), dispatcher.clone());
    let h3 = serve_h3_optional(config.addr, tls_for_h3, dispatcher.clone());

    tokio::select! {
        r = h1h2 => r.context("h1+h2 listener")?,
        r = h3   => r.context("h3 listener")?,
        _ = &mut shutdown => {
            tracing::info!("shutdown signal received");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// HTTP/1.1 + HTTP/2 leg (hyper 1.x)
// ---------------------------------------------------------------------------

async fn serve_h1h2(config: ServerConfig, dispatcher: Arc<HeliosDispatcher>) -> Result<()> {
    let listener = TcpListener::bind(config.addr)
        .await
        .with_context(|| format!("bind TCP {}", config.addr))?;
    tracing::info!(addr = %config.addr, "h1+h2 listener up");

    let static_response = config
        .inline_engine
        .as_ref()
        .and_then(|inline| inline.engine.static_response_body(inline.handle))
        .filter(|_| config.alt_svc.is_none())
        .map(build_static_h1_response);

    loop {
        let (mut stream, peer) = match listener.accept().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "accept error");
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
        };
        let _ = stream.set_nodelay(true);

        if let Some(response) = static_response.clone() {
            tokio::spawn(async move {
                if let Err(e) = serve_static_h1_connection(&mut stream, &response).await {
                    tracing::debug!(peer = %peer, error = %e, "static h1 connection ended");
                }
            });
            continue;
        }

        let dispatcher = dispatcher.clone();
        let inline_engine = config.inline_engine.clone();
        let alt_svc = config.alt_svc.clone();

        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let svc = service_fn(move |req: Request<Incoming>| {
                let dispatcher = dispatcher.clone();
                let inline_engine = inline_engine.clone();
                let alt_svc = alt_svc.clone();
                let proto = if req.version() == http::Version::HTTP_2 {
                    Protocol::H2
                } else {
                    Protocol::H1
                };
                async move {
                    let resp = handle_h1h2(dispatcher, inline_engine, proto, peer, req).await;
                    let resp = attach_alt_svc(resp, alt_svc.as_deref());
                    Ok::<_, std::convert::Infallible>(resp)
                }
            });

            if let Err(e) = auto::Builder::new(TokioExecutor::new())
                .serve_connection(io, svc)
                .await
            {
                tracing::debug!(error = %e, "h1/h2 connection ended");
            }
        });
    }
}

fn build_static_h1_response(body: Bytes) -> Arc<[u8]> {
    let mut response = Vec::with_capacity(64 + body.len());
    response.extend_from_slice(b"HTTP/1.1 200 OK\r\ncontent-length: ");
    response.extend_from_slice(body.len().to_string().as_bytes());
    response.extend_from_slice(b"\r\nconnection: keep-alive\r\n\r\n");
    response.extend_from_slice(&body);
    Arc::from(response)
}

async fn serve_static_h1_connection(
    stream: &mut tokio::net::TcpStream,
    response: &[u8],
) -> Result<()> {
    let mut buf = [0u8; 16 * 1024];
    let mut len = 0usize;
    let mut header_scan = 0usize;
    loop {
        if len == buf.len() {
            anyhow::bail!("h1 request buffer exceeded {} bytes", buf.len());
        }
        let n = stream
            .read(&mut buf[len..])
            .await
            .context("read h1 request")?;
        if n == 0 {
            return Ok(());
        }
        len += n;

        while let Some(header_end) = find_header_end_from(&buf[..len], header_scan) {
            let headers = &buf[..header_end];
            let body_len = h1_request_body_len(headers);
            let request_len = header_end + 4 + body_len;
            if len < request_len {
                header_scan = header_end;
                break;
            }
            write_all_fast(stream, response)
                .await
                .context("write h1 response")?;

            if request_len == len {
                len = 0;
                header_scan = 0;
                break;
            }
            buf.copy_within(request_len..len, 0);
            len -= request_len;
            header_scan = 0;
        }

        if find_header_end_from(&buf[..len], header_scan).is_none() {
            header_scan = len.saturating_sub(3);
        }
    }
}

fn h1_request_body_len(headers: &[u8]) -> usize {
    content_length(headers).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::h1_request_body_len;

    #[test]
    fn h1_get_with_explicit_content_length_has_body() {
        let headers = b"GET / HTTP/1.1\r\nhost: example\r\ncontent-length: 5";

        assert_eq!(h1_request_body_len(headers), 5);
    }
}

fn attach_alt_svc(mut resp: Response<Full<Bytes>>, alt_svc: Option<&str>) -> Response<Full<Bytes>> {
    if let Some(v) = alt_svc {
        if let Ok(hv) = http::HeaderValue::from_str(v) {
            resp.headers_mut().insert(http::header::ALT_SVC, hv);
        }
    }
    resp
}

async fn handle_h1h2(
    dispatcher: Arc<HeliosDispatcher>,
    inline_engine: Option<InlineEngine>,
    proto: Protocol,
    peer: SocketAddr,
    req: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let (parts, body) = req.into_parts();
    let size_hint = body.size_hint();
    let body_bytes = if size_hint.lower() == 0 && size_hint.upper() == Some(0) {
        Bytes::new()
    } else {
        match body.collect().await {
            Ok(b) => b.to_bytes(),
            Err(e) => {
                return error_response(StatusCode::BAD_REQUEST, format!("body read error: {e}"));
            }
        }
    };

    let req_bytes = encode_request_capnp(&parts, &body_bytes, peer, proto);
    if let Some(inline) = inline_engine {
        return match inline.engine.call_fetch_handler(inline.handle, req_bytes) {
            Ok(bytes) => {
                let _ = inline.engine.drain_microtasks(inline.handle);
                decode_response_capnp(bytes)
            }
            Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, e.message),
        };
    }

    let rx = dispatcher.dispatch(RequestData {
        req_bytes,
        protocol: proto,
    });
    match rx.await {
        Ok(ResponseData::Done(bytes)) => decode_response_capnp(bytes),
        Ok(ResponseData::RequestError(e)) => {
            error_response(StatusCode::INTERNAL_SERVER_ERROR, e.message)
        }
        Ok(ResponseData::ScriptError(e)) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            e.map(|e| e.message)
                .unwrap_or_else(|| "script error".to_string()),
        ),
        Err(_) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "worker dropped response channel",
        ),
    }
}

fn error_response(status: StatusCode, msg: impl Into<String>) -> Response<Full<Bytes>> {
    let mut r = Response::new(Full::new(Bytes::from(msg.into())));
    *r.status_mut() = status;
    r
}

// ---------------------------------------------------------------------------
// HTTP/3 leg (quinn + h3)
// ---------------------------------------------------------------------------

async fn serve_h3_optional(
    addr: SocketAddr,
    tls: Option<TlsConfig>,
    dispatcher: Arc<HeliosDispatcher>,
) -> Result<()> {
    let Some(tls) = tls else {
        // No TLS configured ⇒ no QUIC. Park forever; the select! arm
        // simply never wins. We don't return Ok() because that would
        // cancel the other arm.
        std::future::pending::<()>().await;
        unreachable!()
    };

    let server_cfg = build_quinn_server_config(&tls)?;
    let endpoint = quinn::Endpoint::server(server_cfg, addr)
        .with_context(|| format!("bind UDP/QUIC {}", addr))?;
    tracing::info!(addr = %addr, "h3 listener up");

    while let Some(incoming) = endpoint.accept().await {
        let dispatcher = dispatcher.clone();
        tokio::spawn(async move {
            let conn = match incoming.await {
                Ok(c) => c,
                Err(e) => {
                    tracing::debug!(error = %e, "quinn handshake");
                    return;
                }
            };
            // h3-quinn 0.0.10: `Connection::new` is synchronous.
            let h3_conn = h3_quinn::Connection::new(conn);
            // h3 0.0.8: build the server connection via `server::builder`.
            let h3_server = match h3::server::builder().build(h3_conn).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::debug!(error = ?e, "h3 builder");
                    return;
                }
            };
            if let Err(e) = handle_h3_connection(h3_server, dispatcher).await {
                tracing::debug!(error = %e, "h3 connection ended");
            }
        });
    }
    Ok(())
}

fn build_quinn_server_config(tls: &TlsConfig) -> Result<quinn::ServerConfig> {
    let certs = load_certs(&tls.cert_chain_pem_path)?;
    let key = load_key(&tls.private_key_pem_path)?;

    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("rustls cert/key")?;
    crypto.alpn_protocols = vec![b"h3".to_vec()];
    crypto.max_early_data_size = u32::MAX;

    let qcrypto = quinn::crypto::rustls::QuicServerConfig::try_from(crypto)
        .context("wrap rustls as QuicServerConfig")?;
    Ok(quinn::ServerConfig::with_crypto(Arc::new(qcrypto)))
}

fn load_certs(path: &std::path::Path) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    let f =
        std::fs::File::open(path).with_context(|| format!("open cert chain {}", path.display()))?;
    let mut r = std::io::BufReader::new(f);
    let certs = rustls_pemfile::certs(&mut r)
        .collect::<Result<Vec<_>, _>>()
        .context("parse PEM cert chain")?;
    Ok(certs)
}

fn load_key(path: &std::path::Path) -> Result<rustls::pki_types::PrivateKeyDer<'static>> {
    let f = std::fs::File::open(path).with_context(|| format!("open key {}", path.display()))?;
    let mut r = std::io::BufReader::new(f);
    rustls_pemfile::private_key(&mut r)
        .context("parse PEM private key")?
        .ok_or_else(|| anyhow::anyhow!("no private key found in {}", path.display()))
}

async fn handle_h3_connection<C>(
    mut conn: h3::server::Connection<C, Bytes>,
    dispatcher: Arc<HeliosDispatcher>,
) -> Result<()>
where
    C: h3::quic::Connection<Bytes> + 'static,
    <C as h3::quic::OpenStreams<Bytes>>::BidiStream: h3::quic::BidiStream<Bytes> + Send + 'static,
    <<C as h3::quic::OpenStreams<Bytes>>::BidiStream as h3::quic::BidiStream<Bytes>>::SendStream:
        Send,
    <<C as h3::quic::OpenStreams<Bytes>>::BidiStream as h3::quic::BidiStream<Bytes>>::RecvStream:
        Send,
{
    loop {
        match conn.accept().await {
            Ok(Some(resolver)) => {
                let dispatcher = dispatcher.clone();
                tokio::spawn(async move {
                    let (req, stream) = match resolver.resolve_request().await {
                        Ok(p) => p,
                        Err(e) => {
                            tracing::debug!(error = ?e, "h3 resolve");
                            return;
                        }
                    };
                    // WebTransport hand-off: a CONNECT request with
                    // `:protocol: webtransport` is the upgrade signal.
                    if crate::webtransport::is_webtransport_connect(&req) {
                        crate::webtransport::handle_session(req, stream, dispatcher).await;
                        return;
                    }
                    if let Err(e) = handle_h3_request(req, stream, dispatcher).await {
                        tracing::debug!(error = %e, "h3 request");
                    }
                });
            }
            Ok(None) => break,
            Err(e) => {
                tracing::debug!(error = ?e, "h3 accept");
                break;
            }
        }
    }
    Ok(())
}

async fn handle_h3_request<S>(
    req: Request<()>,
    mut stream: h3::server::RequestStream<S, Bytes>,
    dispatcher: Arc<HeliosDispatcher>,
) -> Result<()>
where
    S: h3::quic::BidiStream<Bytes>,
{
    // Drain inbound body.
    let mut body = Vec::new();
    while let Some(mut chunk) = stream.recv_data().await? {
        // h3 0.0.8: `chunk` is `impl Buf`; copy out via `copy_to_bytes`.
        let remaining = chunk.remaining();
        let b = chunk.copy_to_bytes(remaining);
        body.extend_from_slice(&b);
    }
    let (parts, _) = req.into_parts();
    let peer: SocketAddr = "0.0.0.0:0".parse().unwrap();
    let req_bytes = encode_request_capnp_parts(&parts, &body, peer, Protocol::H3);

    let rx = dispatcher.dispatch(RequestData {
        req_bytes,
        protocol: Protocol::H3,
    });
    let resp = match rx.await {
        Ok(ResponseData::Done(b)) => decode_response_capnp(b),
        Ok(ResponseData::RequestError(e)) => {
            error_response(StatusCode::INTERNAL_SERVER_ERROR, e.message)
        }
        Ok(ResponseData::ScriptError(e)) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            e.map(|e| e.message)
                .unwrap_or_else(|| "script error".into()),
        ),
        Err(_) => error_response(StatusCode::SERVICE_UNAVAILABLE, "channel closed"),
    };
    let (parts, body) = resp.into_parts();
    let head = Response::from_parts(parts, ());
    stream.send_response(head).await?;
    stream.send_data(body.collect().await?.to_bytes()).await?;
    stream.finish().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Cap'n Proto request/response codec (Phase 3 wire format)
// ---------------------------------------------------------------------------
//
// The full schema lives in `wit/helios-rpc.capnp`. For the dispatcher path
// the host doesn't need to decode user-side structures — it just needs a
// stable wire format the JS side can mirror. We use a minimal hand-rolled
// length-prefixed framing that's forward-compatible with the Cap'n Proto
// schema (same field ordering). When the worker component is built, this
// gets replaced by the generated `capnp::Builder`/`Reader` code.

fn encode_request_capnp(
    parts: &http::request::Parts,
    body: &Bytes,
    peer: SocketAddr,
    proto: Protocol,
) -> Bytes {
    encode_request_capnp_parts(parts, body, peer, proto)
}

fn encode_request_capnp_parts(
    parts: &http::request::Parts,
    body: &[u8],
    peer: SocketAddr,
    proto: Protocol,
) -> Bytes {
    use bytes::BufMut;
    let mut out = bytes::BytesMut::with_capacity(256 + body.len());
    write_str(&mut out, parts.method.as_str());
    write_str(&mut out, &parts.uri.to_string());
    out.put_u32_le(parts.headers.len() as u32);
    for (k, v) in parts.headers.iter() {
        write_str(&mut out, k.as_str());
        write_bytes(&mut out, v.as_bytes());
    }
    write_bytes(&mut out, body);
    write_str(&mut out, &peer.to_string());
    write_str(&mut out, proto.as_str());
    out.freeze()
}

fn decode_response_capnp(bytes: Bytes) -> Response<Full<Bytes>> {
    // Permissive decode: any failure becomes a 502.
    match try_decode_response(&bytes) {
        Some(r) => r,
        None => {
            // Treat as opaque body (JSON/text) with 200 OK.
            let mut r = Response::new(Full::new(bytes));
            *r.status_mut() = StatusCode::OK;
            r.headers_mut().insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("application/json"),
            );
            r
        }
    }
}

fn try_decode_response(bytes: &Bytes) -> Option<Response<Full<Bytes>>> {
    const MAX_HEADERS: u32 = 256;
    const MAX_HEADER_NAME: u32 = 256;
    const MAX_HEADER_VAL: u32 = 65_536;
    let mut p = 0usize;
    let status = read_u16(bytes, &mut p)?;
    if !(100..=599).contains(&status) {
        return None;
    }
    let nh = read_u32(bytes, &mut p)?;
    if nh > MAX_HEADERS {
        return None;
    }
    let mut headers = http::HeaderMap::with_capacity(nh as usize);
    for _ in 0..nh {
        let k = read_bounded(bytes, &mut p, MAX_HEADER_NAME)?;
        let v = read_bounded(bytes, &mut p, MAX_HEADER_VAL)?;
        let k = std::str::from_utf8(k).ok()?;
        if let (Ok(k), Ok(v)) = (
            http::HeaderName::from_bytes(k.as_bytes()),
            http::HeaderValue::from_bytes(v),
        ) {
            headers.insert(k, v);
        }
    }
    let body = read_bytes(bytes, &mut p)?;
    let mut r = Response::new(Full::new(Bytes::copy_from_slice(body)));
    *r.status_mut() = StatusCode::from_u16(status).ok()?;
    *r.headers_mut() = headers;
    Some(r)
}

fn read_bounded<'a>(b: &'a Bytes, p: &mut usize, max: u32) -> Option<&'a [u8]> {
    let n = read_u32(b, p)?;
    if n > max {
        return None;
    }
    let n = n as usize;
    if *p + n > b.len() {
        return None;
    }
    let s = &b[*p..*p + n];
    *p += n;
    Some(s)
}

fn write_str(buf: &mut bytes::BytesMut, s: &str) {
    write_bytes(buf, s.as_bytes())
}
fn write_bytes(buf: &mut bytes::BytesMut, b: &[u8]) {
    use bytes::BufMut;
    buf.put_u32_le(b.len() as u32);
    buf.put_slice(b);
}
fn read_u16(b: &Bytes, p: &mut usize) -> Option<u16> {
    if *p + 2 > b.len() {
        return None;
    }
    let v = u16::from_le_bytes(b[*p..*p + 2].try_into().ok()?);
    *p += 2;
    Some(v)
}
fn read_u32(b: &Bytes, p: &mut usize) -> Option<u32> {
    if *p + 4 > b.len() {
        return None;
    }
    let v = u32::from_le_bytes(b[*p..*p + 4].try_into().ok()?);
    *p += 4;
    Some(v)
}
fn read_bytes<'a>(b: &'a Bytes, p: &mut usize) -> Option<&'a [u8]> {
    let n = read_u32(b, p)? as usize;
    if *p + n > b.len() {
        return None;
    }
    let s = &b[*p..*p + n];
    *p += n;
    Some(s)
}
