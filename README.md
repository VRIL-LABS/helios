<p align="center">
  <img src="assets/helios-header.svg" alt="Helios — JavaScript runtime infrastructure for Workers, HTTP/3, WebTransport, and WASM snapshots" width="100%">
</p>

# ☀️ Helios
> **JIT at the edge, blazing like the sun.**
> ***WinterJS promised the sun. Helios delivers it.***

Helios is a high-performance Rust runtime for Workers-style JavaScript at the edge. It delivers **80,000–117,000 requests per second** on a single node — making it one of the fastest, if not *the* fastest, JavaScript-capable server runtime available.

Built around a native host architecture, Helios provides fast request dispatch across multiple workers, a shared bytecode cache for near-zero startup latency, HTTP/3 (QUIC) transport, WebTransport routing, and Wizer-based WASM snapshot pre-initialization.

## ⚡ Performance

| Runtime | Req/s (HTTP/1.1, 64 conns) |
|---|---|
| **Helios v1.0-alpha** | **80,000 – 117,000** |
| Bun | ~90,000 |
| Node.js | ~50,000 |

> Benchmarks run with `helios bench` on an AMD Ryzen 9 (Linux x86-64, kernel 6.x), 15 s duration, 2 s warmup, 64 concurrent connections, loopback interface. Run `helios bench` on your own hardware using the scripts in `bench/` for reproducible numbers.

## Features

| Feature | Description |
|---|---|
| Multi-worker dispatcher | Round-robin, least-loaded, and power-of-two routing policies |
| Shared bytecode cache | XDR cache shares compiled blobs across workers for reduced startup latency |
| HTTP/1.1, HTTP/2 & HTTP/3 | Full Hyper 1.x stack; HTTP/3 over QUIC with a TLS cert/key |
| WebTransport | HTTP/3 CONNECT-based routing for bidirectional streams |
| WASM snapshots | Wizer-based pre-initialized snapshots for instant worker startup |
| Built-in benchmarking | Closed-loop load generator with HDR-histogram latency reporting |

## Quick start

Download the latest binary from the [Releases](https://github.com/VRIL-LABS/helios/releases) page:

```sh
# Download and extract (replace URL with the latest release asset)
curl -L https://github.com/VRIL-LABS/helios/releases/download/v1.0-alpha/helios-v1.0-alpha-linux-x86_64.tar.gz \
  | tar -xz
./helios serve bench/helios-simple.js --port 8080 --workers 4
```

Or build from source:

```sh
git clone https://github.com/VRIL-LABS/helios
cd helios/rs
cargo build --release -p helios
./target/release/helios serve ../bench/helios-simple.js --port 8080 --workers 4
```

## CLI reference

```text
helios serve <app.js> [--port 8080] [--workers N] [--policy power-of-two]
                      [--cert PATH --key PATH] [--alt-svc 'h3=":8443"; ma=86400']
helios build <app.js> -o app.wasm [--worker-wasm helios-worker.wasm] [--no-opt]
helios bench <url>    [-d 30] [-c 64] [-R 50000] [--http auto|http1|http2|http3] [--json]
helios exec  <script.js>
```

**Notes:**
* HTTP/3 requires TLS (QUIC mandates TLS 1.3). Pass `--cert` and `--key` to enable it.
* `helios build` requires `wizer` on `PATH`; pass `--no-opt` to skip `wasm-opt`.

## Benchmarking

Helios ships with a built-in benchmark harness — no external tooling required:

```sh
# Serve
./helios serve bench/helios-simple.js --port 8080 --workers 4

# Benchmark (separate terminal)
RUST_LOG=warn ./helios bench http://127.0.0.1:8080/ \
  --duration 15 \
  --warmup 2 \
  --connections 64 \
  --rate 100000 \
  --json
```

The harness supports configurable duration, warmup, connection count, and target rate with coordinated-omission correction and HDR-histogram latency tracking.

To compare against Node.js or Bun, run `bench/node-simple.js` or `bench/bun-simple.js` and use the same `helios bench` flags.

## Releasing

The [Release: v1.0-alpha](.github/workflows/release-v1-alpha.yml) workflow builds the Linux binary, runs the test suite, packages a `tar.gz` archive with `README.md`, `LICENSE`, and benchmark scripts, and publishes a GitHub release — all in one step. Trigger it from **Actions → Release: v1.0-alpha → Run workflow**.

## License

Helios is licensed under the MIT License. See [`LICENSE`](LICENSE).

*"WinterJS promised the sun. Helios delivers it."*
