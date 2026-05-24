# HELIOS RPC schema — Cap'n Proto wire format between WASM worker and host engine.
#
# Phase 3: the WASM component serializes Request/Response across the
# `helios:engine/js-engine` WIT boundary as `list<u8>` blobs encoded with
# this schema. Cap'n Proto is chosen for zero-copy decode at the host side.

@0xa1b2c3d4e5f60718;

struct Header {
    name @0 :Text;
    value @1 :Data;
}

struct Request {
    method @0 :Text;
    url @1 :Text;
    headers @2 :List(Header);
    body @3 :Data;
    # Original socket peer address ("ip:port"), for X-Forwarded-For and
    # request.cf.colo emulation.
    peer @4 :Text;
    # Inbound protocol: "h1", "h2", or "h3".
    protocol @5 :Text;
}

struct Response {
    status @0 :UInt16;
    headers @1 :List(Header);
    body @2 :Data;
    # Set when the JS handler returned a streaming ReadableStream; the host
    # then pulls additional chunks via a follow-up `pull-body` call.
    streaming @3 :Bool;
    streamHandle @4 :UInt32;
}

struct JsError {
    message @0 :Text;
    stack @1 :Text;
    location @2 :Text;
}
