<p align="center">
  <img src="assets/helios-header.svg" alt="Helios — JavaScript runtime infrastructure for Workers, HTTP/3, WebTransport, and WASM snapshots" width="100%">
</p>

# ☀️ Helios
> **JIT at the edge, blazing like the sun.**

Helios is a Rust-based HTTP server and toolchain for running Workers-style JavaScript handlers at the edge. It provides multi-protocol HTTP serving (HTTP/1.1, HTTP/2, and HTTP/3), a built-in load-testing tool, and a Wizer-based WASM snapshot pipeline.

> **Status: v1.1.0-beta.** The current release ships the full server, dispatch, and benchmarking infrastructure with JavaScript execution powered by the Boa ECMAScript engine. Full SpiderMonkey JIT integration is in active development for maximum performance.

## Quick start

### Using the prebuilt binary

A precompiled Linux (x86_64) binary is included at `bin/helios`.

```sh
# Serve a Workers-style JS handler
./bin/helios serve bench/helios-simple.js --port 8080

# In another terminal
curl http://127.0.0.1:8080/
```

### Build from source

Requires Rust 1.75 or newer.

```sh
cd rs
cargo build --release
cd ..
./rs/target/release/helios serve bench/helios-simple.js --port 8080
```

## Writing a handler

Helios uses the Workers `fetch` event API:

```js
addEventListener('fetch', (event) => {
  event.respondWith(new Response('Hello from Helios!'));
});
```

> **Note:** Full SpiderMonkey JIT execution is in development. The current build uses the Boa ECMAScript engine, which executes your handler JavaScript and returns the actual response body. SpiderMonkey JIT will provide higher throughput once integrated.

See [`bench/helios-simple.js`](bench/helios-simple.js) for a minimal example and [`bench/complex.js`](bench/complex.js) for a more involved one.

## CLI reference

```text
helios serve <app.js> [-s] [--port 8080] [--workers N] [--policy round-robin|least-loaded|power-of-two]
                      [--ip ADDR] [--cert PATH --key PATH]
                      [--alt-svc 'h3=":8443"; ma=86400']
                      [--shutdown-timeout 60]

helios build <app.js> [-o app.wasm] [--worker-wasm helios-worker.wasm]
                      [--target wasip2] [--no-opt]

helios bench <url>    [-d 30] [--warmup 3] [-c 64] [-R 50000]
                      [--http auto|http1|http2|http3]
                      [--body-file FILE] [--json]

helios exec  <script.js> [-s]
```

**Notes:**

- `helios serve` fans requests across worker threads using a lock-free dispatcher. Default dispatch policy is `power-of-two`.
- `-s` / `--script` runs the JS file in script mode (no module resolution). Applies to both `helios serve` and `helios exec`.
- HTTP/3 requires a TLS certificate and key (`--cert` / `--key`) because QUIC mandates TLS 1.3. The `--alt-svc` flag sets the `Alt-Svc` response header to advertise the H3 endpoint to clients.
- `helios build` invokes [Wizer](https://github.com/bytecodealliance/wizer) to produce a pre-initialized WASM snapshot. Requires `wizer` on `PATH` and a compatible `helios-worker.wasm` component. `wasm-opt` is also required unless `--no-opt` is passed.
- `helios exec` runs a script once using the Boa JS engine and exits.

## Benchmarking

`helios bench` is a built-in closed-loop load generator with HDR-histogram latency tracking and optional coordinated-omission correction.

```sh
# Start the server
./bin/helios serve bench/helios-simple.js --port 8080

# Run a benchmark
./bin/helios bench http://127.0.0.1:8080/ \
  --duration 15 \
  --warmup 2 \
  --connections 64 \
  --rate 20000 \
  --json
```

For runtime comparisons, use the provided parity servers with the same flags:

```sh
# These servers also listen on port 8080; stop helios (or use a different port) before running them.
node bench/node-simple.js   # port 8080
bun  bench/bun-simple.js    # port 8080
```

## Repository contents

| Path | Contents |
|---|---|
| `bin/helios` | Precompiled Linux x86_64 binary. |
| `rs/` | Rust workspace source (`helios` crate). |
| `bench/` | JS handler fixtures and Node/Bun comparison servers. |
| `assets/` | Project graphics. |
| `LICENSE` | MIT license. |

## License

Helios is licensed under the MIT License. See [`LICENSE`](LICENSE).

*"WinterJS promised the sun. Helios delivers it."*
