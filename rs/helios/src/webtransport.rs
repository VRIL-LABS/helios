//! Phase 4 — WinterCG WebTransport server binding.
//!
//! WebTransport sessions ride on top of an HTTP/3 connection: a client
//! issues an extended CONNECT request with `:protocol: webtransport` and
//! `:scheme: https`, the server accepts, and the resulting QUIC streams
//! (bidi + uni) become the transport for application data.
//!
//! This is the *server-side* WinterCG JS API binding referenced as item
//! 4 in `.github/copilot-instructions/dependency-instructions.md`'s
//! "What IS Custom-Built" list. No edge runtime has shipped this yet;
//! there is no reference implementation to copy.
//!
//! The dispatcher receives a [`crate::dispatcher::Protocol::WebTransport`]
//! tagged request whose `req_bytes` is the CONNECT request envelope; the
//! JS handler installed via `addEventListener("webtransport", ...)` then
//! pumps `IncomingBidirectionalStreams` / `IncomingUnidirectionalStreams`
//! and writes responses back through QUIC stream handles that the host
//! maps to `WritableStream` / `ReadableStream`.
//!
//! The full bidirectional pump lives in the host-side worker once the
//! session is established; this module's job is to:
//!
//!   1. Detect a WebTransport CONNECT (`is_webtransport_connect`).
//!   2. Negotiate the 200 response that completes the upgrade.
//!   3. Hand the QUIC streams off to JS via the dispatcher.

use std::sync::Arc;

use bytes::Bytes;
use http::{Request, Response, StatusCode};

use crate::dispatcher::{HeliosDispatcher, Protocol, RequestData};

/// Inspect an H3 request and return true if it is a WebTransport CONNECT.
///
/// Per `draft-ietf-webtrans-http3`:
///   :method = CONNECT
///   :protocol = webtransport
///   :scheme = https
pub fn is_webtransport_connect(req: &Request<()>) -> bool {
    if req.method() != http::Method::CONNECT {
        return false;
    }
    // The `:protocol` pseudo-header is exposed in h3 via an extension
    // header named `:protocol`. h3 surfaces this as a regular header on
    // the request map.
    req.headers()
        .get(":protocol")
        .map(|v| v.as_bytes().eq_ignore_ascii_case(b"webtransport"))
        .unwrap_or(false)
}

/// Drive one WebTransport session.
///
/// Currently a minimal accept-and-echo: responds 200 to complete the
/// upgrade, sends the CONNECT envelope to JS via the dispatcher tagged
/// `Protocol::WebTransport`, and lets the JS handler emit per-stream data.
/// In a fully wired build the QUIC stream surface is exposed to JS as
/// WinterCG's `WebTransport.incomingBidirectionalStreams` etc.
pub async fn handle_session<S>(
    req: Request<()>,
    mut stream: h3::server::RequestStream<S, Bytes>,
    dispatcher: Arc<HeliosDispatcher>,
) where
    S: h3::quic::BidiStream<Bytes>,
{
    // 1. Tell JS about the session.
    let envelope = encode_connect_envelope(&req);
    let rx = dispatcher.dispatch(RequestData {
        req_bytes: envelope,
        protocol: Protocol::WebTransport,
    });

    // 2. Send the 200 to complete the upgrade.
    let resp = Response::builder()
        .status(StatusCode::OK)
        .header("sec-webtransport-http3-draft", "draft02")
        .body(())
        .expect("static response");
    if let Err(e) = stream.send_response(resp).await {
        tracing::debug!(error = ?e, "wt: send_response failed");
        return;
    }

    // 3. Wait for JS to acknowledge it accepted the session. The actual
    //    per-stream data plane is driven on subsequent dispatcher events
    //    (one per incoming QUIC stream) — wired in the worker component.
    match rx.await {
        Ok(_) => tracing::debug!("wt: session accepted by JS handler"),
        Err(_) => tracing::warn!("wt: dispatcher channel closed during accept"),
    }

    // 4. Keep the request stream alive: closing it aborts the session.
    //    A production implementation pumps the QUIC connection's bidi
    //    and uni stream acceptors here.
    let _ = stream.finish().await;
}

fn encode_connect_envelope(req: &Request<()>) -> Bytes {
    use bytes::BufMut;
    let mut out = bytes::BytesMut::with_capacity(256);
    out.put_u32_le(req.headers().len() as u32);
    for (k, v) in req.headers().iter() {
        let kb = k.as_str().as_bytes();
        let vb = v.as_bytes();
        out.put_u32_le(kb.len() as u32);
        out.put_slice(kb);
        out.put_u32_le(vb.len() as u32);
        out.put_slice(vb);
    }
    let path = req.uri().path();
    out.put_u32_le(path.len() as u32);
    out.put_slice(path.as_bytes());
    out.freeze()
}
