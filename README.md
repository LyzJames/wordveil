# 律金智库 (Lvjin)

A Windows desktop assistant for personal legal and financial documents. All
import, OCR, speech recognition, retrieval, verification and calculation run on
the local computer; documents are stored encrypted (SQLCipher). Generated
answers use a model service the user connects and approves per request.

The M3 implementation (0.4.0) retains all 30 skill handlers, approved
DeepSeek Search/Fetch, versioned official public knowledge packages, a
read-only MCP client and the independent `lvjin-public-mcp` business service.
The public service uses only the six bundled mainland Chinese official texts;
it never opens the user's encrypted vault. External operations require their
own immutable approval, and arbitrary local stdio programs are not enabled.
Installation and completion evidence is kept in the documentation workspace.

M3 adds version-bound document/question comparisons with one batch approval,
sequential cell progress and explicit failed-cell recovery. Request counters
survive interruption; completed cells are preserved. Retrieval supports frozen
dates and mainland regions, plus source-backed graph paths of at most two hops.
Graph expansion remains disabled by default because the fixed small holdout
did not show an improvement. F32 encrypted index partitions use a bounded
cache; long indexing jobs persist completed batches and publish atomically.

The M3 release also pins Pillow 13.0.0, certifi 2026.10.19 and
webpki-root-certs 1.0.10 for reviewed security fixes. Model artefacts and CPU
FP32 inference remain fixed. Test results, limits and actual installed-media
acceptance are recorded separately; implementation does not imply every
release gate has passed.

Post-acceptance source fixes (2026-10-24) add a vault-scoped Q&A history
browser with search, mainland date filters and cursor pagination. Opening a
saved answer, citation or feedback is read-only and never restarts generation.
The financial engine reports one annual rate only when a verified root and
one cash-flow sign change establish uniqueness; a bounded grid cannot certify
other cases. Saved calculations with unproven uniqueness are guarded on read,
preserving the original encrypted audit record. The previously accepted M3
installation media remains a separate, unchanged historical deliverable.

## Layout

| Path | Contents |
| --- | --- |
| `src/` | React interface (TypeScript), bundled with esbuild |
| `src-tauri/` | Trusted Rust core (`lvjin_core` library) and the Tauri desktop binary `lvjin` |
| `agent_core/` | Python worker: local model owners, document parsing and the LangGraph orchestrator |
| `resources/` | Locked runtime resources (skills, prompts, contracts, model and installation manifests; hashes in `resource-lock.json`) |
| `worker.spec` | PyInstaller onedir specification of the frozen worker |
| `installer/` | Inno Setup scripts for the offline media |

Tests, benchmarks, validation programs, fixtures, build tooling and reports
live in the separate documentation workspace (`docs/`, with `docs/tests/Cargo.toml`
building the tests against this library). This repository builds without it.

## Building the desktop binary

Requirements: Windows 11 x64, the stable Rust toolchain with MSVC Build Tools,
Node.js with npm, and a static OpenSSL 3.5 LTS build for SQLCipher.

1. OpenSSL 3.5 LTS (static, `libcrypto.lib`), for example from the official
   source with Strawberry Perl and the Visual C++ developer prompt:
   `perl Configure VC-WIN64A --prefix=<dir> --libdir=lib no-shared no-module no-tests no-comp no-zlib no-zlib-dynamic no-legacy no-ssl3 no-md2 no-rc5 no-weak-ssl-ciphers no-camellia no-idea no-seed no-capieng no-asm`,
   then `nmake build_libs` and `nmake install_dev`.
2. Environment for the Rust build:
   - `OPENSSL_DIR=<dir>` (the build checks for `lib/libcrypto.lib` and the headers);
   - `LIBSQLITE3_FLAGS=-DSQLCIPHER_OMIT_LOG` (SQLCipher's internal logging must be compiled out).
3. Frontend: `npm ci`, `npm run typecheck`, `npm run build` (writes `dist/`).
4. Core and desktop binary:
   `cargo build --release --manifest-path src-tauri/Cargo.toml --features desktop --bin lvjin`.

For a full production image, first build `--bin lvjin-public-mcp`, stage that
fixed executable, the frozen worker and all runtime/model files, and regenerate
the installation and resource hashes. Build the desktop core against those
final anchors. Rebuilding a staged runtime afterwards requires refreshing its
inventory and rebuilding the desktop core. The documentation workspace keeps
the exact build scripts, SBOM, source archive and offline installer pipeline.

The binary expects the installed layout next to it (`resources/`, the frozen
worker and the nine CPU models under `runtimes/`, the fixed WebView2 runtime and
`data-root.txt`); the release staging and installer steps produce it. Builds
with the `dev-worker` feature run the worker from source using the
documentation workspace instead.
