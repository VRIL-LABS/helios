//! Phase 5 — Wizer snapshot integration.
//!
//! For Wasmer Edge / pure-WASM deployments where the Phase-3 host engine
//! isn't available (because the WASM sandbox cannot grant PROT_EXEC, so
//! the JIT is dead inside), HELIOS still wins via **build-time pre-init**:
//!
//!   1. Compile `helios-worker.wasm` (no JIT needed — purely the WASM
//!      WinterCG surface).
//!   2. Bundle the user's `app.js` into a Wizer init function that calls
//!      `eval-module` and primes the SpiderMonkey module/bytecode cache.
//!   3. Run Wizer over the worker. Wizer drives execution up to the
//!      `--init-func` and snapshots the linear memory.
//!   4. Output `app.wasm` — memory-pre-populated with parsed + bytecoded
//!      app.js. Zero cold-start parse overhead at request time.
//!
//! Two integration paths:
//!
//! * **Subprocess (default)** — invoke the `wizer` binary on `$PATH`.
//!   Works without linking Wizer's heavy build-time dependencies.
//! * **Library (`wizer-lib` feature)** — depend on the `wizer` crate
//!   directly. Builds a self-contained `helios build` with no external
//!   binary requirement.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

/// Build configuration for `helios build`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildConfig {
    /// Path to the user's JS app (file or directory).
    pub js_path: PathBuf,
    /// Path to `helios-worker.wasm` (the pre-built WASM component).
    pub worker_wasm: PathBuf,
    /// Output `.wasm` path.
    pub output: PathBuf,
    /// WASM target. Currently only `wasm32-wasip2` is supported.
    pub target: BuildTarget,
    /// Wizer `--init-func`. Defaults to `helios_init`.
    pub init_func: String,
    /// Whether to run `wasm-opt -O3` after Wizer.
    pub wasm_opt: bool,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum BuildTarget {
    Wasip2,
}

impl BuildTarget {
    pub fn as_str(&self) -> &'static str {
        match self {
            BuildTarget::Wasip2 => "wasm32-wasip2",
        }
    }
}

impl Default for BuildConfig {
    fn default() -> Self {
        Self {
            js_path: PathBuf::from("app.js"),
            worker_wasm: PathBuf::from("helios-worker.wasm"),
            output: PathBuf::from("app.wasm"),
            target: BuildTarget::Wasip2,
            init_func: "helios_init".to_string(),
            wasm_opt: true,
        }
    }
}

/// Run the `helios build` pipeline end-to-end.
pub fn build(cfg: &BuildConfig) -> Result<BuildOutcome> {
    tracing::info!(
        target = cfg.target.as_str(),
        js = %cfg.js_path.display(),
        out = %cfg.output.display(),
        "helios build: starting"
    );

    // Step 1: validate inputs.
    anyhow::ensure!(
        cfg.js_path.exists(),
        "JS source {} does not exist",
        cfg.js_path.display()
    );
    anyhow::ensure!(
        cfg.worker_wasm.exists(),
        "worker WASM {} does not exist (build the helios-worker component first)",
        cfg.worker_wasm.display()
    );

    // Step 2: embed the JS source into the worker as a known section
    // so the init function can find it. We write a sibling `.helios.json`
    // file alongside the input wasm containing the source so the worker
    // can read it via WASI at init time. This sidesteps having to rewrite
    // the wasm section table here.
    let staging_dir = tempdir_for_build()?;
    let init_payload = staging_dir.join("helios_init.json");
    let src = load_user_source(&cfg.js_path)?;
    std::fs::write(
        &init_payload,
        serde_json::to_vec_pretty(&InitPayload {
            module_url: format!(
                "file:///{}",
                cfg.js_path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "app.js".into())
            ),
            source: src,
        })?,
    )?;

    // Step 3: invoke Wizer.
    let pre_opt = staging_dir.join("pre_opt.wasm");
    run_wizer(cfg, &init_payload, &pre_opt)?;

    // Step 4: optionally optimize with wasm-opt.
    if cfg.wasm_opt {
        match run_wasm_opt(&pre_opt, &cfg.output) {
            Ok(()) => tracing::info!("wasm-opt -O3 applied"),
            Err(e) => {
                tracing::warn!(error = %e, "wasm-opt failed; copying unoptimized output");
                std::fs::copy(&pre_opt, &cfg.output)?;
            }
        }
    } else {
        std::fs::copy(&pre_opt, &cfg.output)?;
    }

    let size = std::fs::metadata(&cfg.output)?.len();
    tracing::info!(out = %cfg.output.display(), size_bytes = size,
        "helios build: complete");
    Ok(BuildOutcome {
        output: cfg.output.clone(),
        bytes: size,
    })
}

/// Result of a successful build.
#[derive(Debug, Clone)]
pub struct BuildOutcome {
    pub output: PathBuf,
    pub bytes: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct InitPayload {
    module_url: String,
    source: String,
}

fn load_user_source(path: &Path) -> Result<String> {
    if path.is_dir() {
        for name in ["index.js", "main.js", "worker.js"] {
            let p = path.join(name);
            if p.exists() {
                return Ok(std::fs::read_to_string(&p)?);
            }
        }
        anyhow::bail!("no entry point in {}", path.display())
    } else {
        Ok(std::fs::read_to_string(path)?)
    }
}

fn tempdir_for_build() -> Result<PathBuf> {
    let base = std::env::temp_dir().join("helios-build");
    std::fs::create_dir_all(&base)?;
    let unique = base.join(format!("session-{}", std::process::id()));
    std::fs::create_dir_all(&unique)?;
    Ok(unique)
}

// ---------------------------------------------------------------------------
// Wizer invocation
// ---------------------------------------------------------------------------

#[cfg(not(feature = "wizer-lib"))]
fn run_wizer(cfg: &BuildConfig, init_payload: &Path, out: &Path) -> Result<()> {
    // Subprocess path: shells out to a `wizer` binary on $PATH.
    let mut cmd = Command::new("wizer");
    cmd.arg("--allow-wasi")
        .arg("--wasm-bulk-memory")
        .arg("true")
        .arg("--init-func")
        .arg(&cfg.init_func)
        .arg("--dir")
        .arg(init_payload.parent().unwrap())
        .arg("-o")
        .arg(out)
        .arg(&cfg.worker_wasm);

    tracing::debug!(?cmd, "invoking wizer");
    let status = cmd.status().context(
        "failed to spawn `wizer`; install with `cargo install wizer-cli` \
                  or build with the `wizer-lib` feature",
    )?;
    anyhow::ensure!(status.success(), "wizer exited with status {}", status);
    Ok(())
}

#[cfg(feature = "wizer-lib")]
fn run_wizer(cfg: &BuildConfig, init_payload: &Path, out: &Path) -> Result<()> {
    use wizer::Wizer;
    let wasm = std::fs::read(&cfg.worker_wasm)
        .with_context(|| format!("read {}", cfg.worker_wasm.display()))?;
    let mut w = Wizer::new();
    w.allow_wasi(true)?;
    w.wasm_bulk_memory(true);
    w.init_func(&cfg.init_func);
    w.dir(init_payload.parent().unwrap());
    let snapshot = w.run(&wasm).context("wizer run")?;
    std::fs::write(out, snapshot).with_context(|| format!("write {}", out.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// wasm-opt invocation
// ---------------------------------------------------------------------------

fn run_wasm_opt(input: &Path, output: &Path) -> Result<()> {
    let status = Command::new("wasm-opt")
        .arg("-O3")
        .arg("--enable-bulk-memory")
        .arg(input)
        .arg("-o")
        .arg(output)
        .status()
        .context("spawn `wasm-opt`")?;
    anyhow::ensure!(status.success(), "wasm-opt exited with status {}", status);
    Ok(())
}
