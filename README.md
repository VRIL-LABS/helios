<p align="center">
  <img src="assets/helios-header.svg" alt="Helios — JavaScript runtime infrastructure for Workers, HTTP/3, WebTransport, and WASM snapshots" width="100%">
</p>

# ☀️ Helios
> **JIT at the edge, blazing like the sun.**
> ***WinterJS promised the sun. Helios delivers it.***

Helios is an alpha Rust runtime project descended from `wasmerio/winterjs`. It is rebuilding the Workers-style JavaScript runtime stack around a native host architecture for fast request dispatch, shared bytecode, HTTP/3 transport, WebTransport routing, and Wizer-based WASM snapshots.

The current default build is intentionally self-contained: it uses a pure-Rust `StubEngine` so the dispatcher, XDR cache, HTTP server, benchmark harness, and Wizer packaging path can be built and tested without the SpiderMonkey toolchain. Native SpiderMonkey/JIT support is scaffolded behind the `spidermonkey` feature, but it is not release-ready in this repository yet.

## Repository layout

| Path | Contents |
|---|---|
| `helios/` | Main Rust crate and `helios` CLI. This is the only workspace member. |
| `bench/` | Helios-owned benchmark fixtures and Node/Bun comparison servers. |
| `winterjs-main/` | Upstream WinterJS source retained for reference. It is excluded from the Cargo workspace. |
| `.github/workflows/benchmark.yml` | CI benchmark workflow for the Helios release binary and `helios bench`. |
| `.github/workflows/release-v1-alpha.yml` | Manual GitHub release packaging workflow for the alpha binary. |
| `.github/workflows/publish-public-package.yml` | Manual workflow that stages `/dist` and publishes the public package repository. |

## Current capabilities

| Area | Current status |
|---|---|
| Worker dispatch | `HeliosDispatcher` fans requests out to per-worker channels with round-robin, least-loaded, and power-of-two policies. |
| Bytecode cache | `XdrCache` shares compiled blobs across workers. In the default build these are deterministic `HXDR` stub blobs; real SpiderMonkey XDR is still gated/scaffolded. |
| HTTP server | `helios serve` runs HTTP/1.1 and HTTP/2 over TCP with Hyper 1.x. With a TLS cert/key it also starts the QUIC/H3 path. |
| WebTransport | HTTP/3 CONNECT detection and accept routing are present; full bidirectional stream pumping into JavaScript remains future integration work. |
| WIT host | Wasmtime component-model host scaffolding exists for the `helios:engine/js-engine` interface. |
| Wizer builds | `helios build` invokes Wizer for pre-initialized WASM snapshots when a compatible `helios-worker.wasm` is supplied. |
| Benchmarking | `helios bench` provides a built-in closed-loop load generator with HDR histogram latency reporting and JSON output. |

## CLI

```text
helios serve <app.js> [--port 8080] [--workers N] [--policy power-of-two]
                    [--cert PATH --key PATH] [--alt-svc 'h3=":8443"; ma=86400']
helios build <app.js> -o app.wasm [--worker-wasm helios-worker.wasm] [--no-opt]
helios bench <url> [-d 30] [-c 64] [-R 50000] [--http auto|http1|http2|http3] [--json]
helios exec  <script.js>
```

Notes:

* `helios serve` currently uses `StubEngine` in the default build and returns the stub response body `{"ok":true}` after warming the cache.
* `helios exec` exits with an error unless a future SpiderMonkey-enabled backend is linked.
* `helios build` requires a compatible worker WASM component plus `wizer` on `PATH`; `wasm-opt` is required on `PATH` unless `--no-opt` is passed.
* HTTP/3 requires TLS because QUIC requires TLS 1.3.

## Building and testing

```sh
cd /path/to/helios
cargo test -p helios
cargo build --release -p helios
```

The release binary is written to `target/release/helios`.

## Run Helios locally

Terminal 1:

```sh
cargo build --release -p helios
./target/release/helios serve bench/helios-simple.js --port 8080 --workers 1
```

Terminal 2:

```sh
curl http://127.0.0.1:8080/
```

The default alpha server response is produced by `StubEngine`; treat it as validation of the transport, dispatch, cache, and benchmark paths rather than proof of full JavaScript-handler execution.

## Benchmarks

`helios bench` supports:

* configurable duration, warmup, connection count, and target rate;
* HDR-histogram latency tracking;
* coordinated-omission correction when `--rate` is provided;
* human-readable or JSON output.

Example:

```sh
RUST_LOG=warn ./target/release/helios bench http://127.0.0.1:8080/ \
  --duration 15 \
  --warmup 2 \
  --connections 64 \
  --rate 20000 \
  --json
```

For local comparisons, run the parity servers in `bench/node-simple.js` or `bench/bun-simple.js` and use the same `helios bench` flags for each runtime. The comparison servers are intentionally small smoke-test targets and do not produce byte-for-byte identical responses to the Helios stub backend.

## Release packaging

Two manual workflows are available:

* `Release: Build & Publish Binary` builds, tests, packages the Linux binary with `README.md`, `LICENSE`, and `bench/`, then publishes or updates a GitHub release.
* `Release: Stage & Push Public Package` builds, tests, creates a GitHub release, optionally publishes to crates.io and npm, and pushes the staged package contents to `VRIL-LABS/helios`.

The `Release: Stage & Push Public Package` workflow accepts the following inputs:

| Input | Default | Description |
|---|---|---|
| `release_version` | `v1.0.0-alpha` | SemVer version label (leading `v` stripped automatically for Cargo/npm). |
| `target_repository` | `VRIL-LABS/helios` | Repository that receives the staged git package. |
| `target_branch` | `main` | Branch in the target repository to update. |
| `prerelease` | `true` | Whether to mark the GitHub release as a prerelease. |
| `publish_crates_io` | `false` | When `true`, publishes the `helios` crate to crates.io (requires `CARGO_REGISTRY_TOKEN` secret). |
| `publish_npm` | `false` | When `true`, publishes `@vril-labs/helios` to the npm registry (requires `NPM_TOKEN` secret). |

Required repository secrets: `PUBLIC_RELEASE_TOKEN` (write access to the target repo), `CARGO_REGISTRY_TOKEN` (crates.io API token, only needed when `publish_crates_io=true`), and `NPM_TOKEN` (npm publish token, only needed when `publish_npm=true`).

## License

Helios is licensed under the MIT License. See [`LICENSE`](LICENSE).

*"WinterJS promised the sun. Helios delivers it."*
