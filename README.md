# 律金智库 (Lvjin)

A Windows desktop assistant for personal legal and financial documents. All
import, OCR, speech recognition, retrieval, verification and calculation run on
the local computer; documents are stored encrypted (SQLCipher). Generated
answers use a model service the user connects and approves per request.

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

The binary expects the installed layout next to it (`resources/`, the frozen
worker and the nine CPU models under `runtimes/`, the fixed WebView2 runtime and
`data-root.txt`); the release staging and installer steps produce it. Builds
with the `dev-worker` feature run the worker from source using the
documentation workspace instead.
