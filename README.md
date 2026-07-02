<p align="center">
  <img src="assets/helios-header.svg" alt="Helios — JavaScript runtime infrastructure for Workers, HTTP/3, WebTransport, and WASM snapshots" width="100%">
</p>

# ☀️ Helios
> **JIT at the edge, blazing like the sun.**

[![Crates.io](https://img.shields.io/crates/v/vrillabs-helios.svg)](https://crates.io/crates/vrillabs-helios)
[![docs.rs](https://img.shields.io/docsrs/vrillabs-helios)](https://docs.rs/vrillabs-helios/1.2.0-rc1/helios/)
[![Crates.io downloads](https://img.shields.io/crates/d/vrillabs-helios.svg)](https://crates.io/crates/vrillabs-helios)
[![License](https://img.shields.io/badge/license-VRIL%20LABS%20Open%20Source-blue.svg)](LICENSE)

Helios is an alpha-stage JavaScript runtime written in Rust for edge and serverless-style workloads. It provides a lock-free worker dispatcher, a shared bytecode cache, Hyper-based HTTP/1.1 + HTTP/2 serving, optional QUIC/HTTP/3 and WebTransport support, and a built-in benchmark harness.

The default build is self-contained: it embeds a pure-Rust Boa JavaScript engine, so it runs without any external JavaScript toolchain. An experimental SpiderMonkey/Ion backend is scaffolded behind a feature flag but is not yet production-ready.

This package (v1.2.0-rc1) contains a prebuilt Linux (x86_64) CLI binary plus the Rust crate source, so you can either run Helios immediately or build it yourself.

## Quick start (prebuilt binary)

```sh
tar -xzf helios-v1.2.0-rc1-x86_64-unknown-linux-gnu.tar.gz
./bin/helios serve bench/helios-simple.js --port 8080
```

```sh
curl http://127.0.0.1:8080/
```

The response is produced by the JavaScript fetch handler in `bench/helios-simple.js`, so this exercises the full request path through the embedded engine.

## Building from source

The `rs/` directory contains the Helios Rust workspace (the `helios` crate) so this package can also be published to crates.io or built locally:

```sh
cd rs
cargo build --release -p helios
./target/release/helios serve ../bench/helios-simple.js --port 8080
```

## Metadata

Helios is published on [crates.io](https://crates.io/crates/vrillabs-helios) as `vrillabs-helios`.

```sh
cargo install vrillabs-helios
```

Running the above command will globally install the `helios` binary.

### Install as library

Run the following Cargo command in your project directory:

```sh
cargo add vrillabs-helios
```

Or add the following line to your `Cargo.toml`:

```toml
vrillabs-helios = "1.2.0-rc1"
```

Full API documentation is available on [docs.rs](https://docs.rs/vrillabs-helios/1.2.0-rc1/helios/).

## Current capabilities

| Area | Current status |
|---|---|
| Worker dispatch | Requests fan out to bounded, lock-free per-worker queues with no global request-path mutex, and worker/runtime threads are best-effort pinned to CPUs on Linux. |
| Bytecode cache | Bytecode compiles once and is shared across workers as immutable, reference-counted entries. |
| JavaScript execution | Uses the embedded Boa engine by default; an experimental SpiderMonkey backend is scaffolded but not production-ready. |
| HTTP server | HTTP/1.1 and HTTP/2 over TCP via Hyper 1.x; supplying a TLS cert/key enables QUIC/HTTP/3. Static responses use vectored writes for larger bodies to avoid extra copies. |
| WebTransport | HTTP/3 CONNECT detection and dispatcher handoff are present; full bidirectional stream pumping into JavaScript is ongoing work. |
| Benchmarking | A built-in closed-loop load generator (`helios bench`) reports HDR-histogram latencies and supports JSON output. |

## CLI

```text
helios serve <app.js|dir> [--script] [--ip 0.0.0.0] [--port 8080]
                         [--workers N] [--policy round-robin|least-loaded|power-of-two]
                         [--cert PATH --key PATH] [--alt-svc 'h3=":8443"; ma=86400']
                         [--shutdown-timeout 60] [--warmup-fetch N]
helios build <app.js|dir> [-o app.wasm] [--worker-wasm helios-worker.wasm]
                          [--target wasip2] [--no-opt]
helios bench <url> [-d 30] [--warmup 3] [-c 64] [-R 50000]
                   [--http auto|http1|http2|http3] [--json] [--body-file PATH]
helios exec  <script.js> [--script]
```

Notes:

* Directory entry points resolve in this order: `index.js`, `main.js`, then `worker.js`.
* HTTP/3 requires TLS because QUIC requires TLS 1.3.
* `--warmup-fetch N` (or `HELIOS_FETCH_WARMUP=N`) proactively invokes each worker's fetch handler `N` times before it accepts traffic, priming JIT-capable backends. It is opt-in and most useful with JIT-tiered backends.

## Performance

Helios is benchmarked with its own built-in `helios bench` load generator against static and dynamic (JavaScript fetch-handler) workloads, covering worker dispatch, the bytecode cache, and the HTTP/1 response path. 

## License

Helios is licensed under the VRIL LABS Open Source License. See [`LICENSE`](LICENSE) or [vril.li/license](https://vril.li/license).

*"WinterJS promised the sun. Helios delivers it."*
