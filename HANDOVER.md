# Camera Project — Handover Note (2026-07-03)

## Division of labor (as of this session)
- **Codex**: internet research, environment/toolchain setup, unblocking install issues
- **Claude**: firmware/server code implementation, code review once environment is unblocked

Codex should resolve the blocker below before any Rust code for `firmware/` gets written, since nothing can compile/flash without a working Xtensa toolchain.

## Project goal (recap)
Battery-powered ESP32-S3 camera: AM312 PIR sensor triggers image capture → ESP32-S3 sends image over WiFi via HTTP POST to a Rust server → server calls Llama vision (Ollama locally in dev, RunPod serverless in prod) → phone notification on detection. Full hardware/stack details are in the original project brief (Freenove ESP32-S3 CAM, 18650 + TP4056 + MT3608 power chain).

## Repo state
`/Users/mac/camera-project/` has `firmware/`, `server/`, `shared/` — all still bare `cargo new` stubs (just `Hello, world!` / default `add()` template). No root workspace `Cargo.toml` exists yet. **No firmware/server code has been written.** We're still at the toolchain-install stage.

**Open decision, not yet made:** whether `firmware` (no_std, Xtensa target) should be a member of the same cargo workspace as `server`/`shared` (std, host target). Mixing no_std cross-target crates with host-target crates in one workspace is a known esp-rs pain point (a workspace-root `.cargo/config.toml` target setting applies to all members). Likely resolution: keep `firmware` as an independent crate that path-depends on `shared`, with `server`+`shared` forming the actual workspace. Confirm with user before wiring this up.

## Where we're stuck: `espup install` fails installing Xtensa Rust

### Attempt 1 (resolved)
First run 404'd on every retry while *downloading* `rust.tar.xz`. Root cause diagnosed: the active `rustc` reported `host: x86_64-apple-darwin` (Rosetta) despite the Mac being Apple Silicon (`uname -m` → `arm64`, `rustup show` default host → `aarch64-apple-darwin`). Espressif's `esp-rs/rust-build` v1.95.0.0 release only ships `aarch64-apple-darwin`, `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc` — **no `x86_64-apple-darwin` asset exists**, so the constructed download URL genuinely 404'd. Verified directly via GitHub API (`api.github.com/repos/esp-rs/rust-build/releases/tags/v1.95.0.0`) and via `curl -sI` on the actual asset URL (aarch64 asset returns 302/exists fine).

Suggested fix given: `rustup set default-host aarch64-apple-darwin` then reinstall the stable toolchain, so `rustc -vV` reports `aarch64-apple-darwin`. **Unconfirmed whether user actually applied this or whether it's what changed behavior below** — worth re-checking `rustc -vV` first.

### Attempt 2 (current blocker — new failure mode)
Latest log shows GCC and LLVM reused from a previous partial install (`~/.rustup/toolchains/esp/xtensa-esp32-elf-clang/esp-20.1.1_20250829`, `~/.rustup/toolchains/esp/xtensa-esp-elf/esp-15.2.0_20250920`). This time **both tarballs download successfully** ("All downloads complete" logged twice — once for `rust-src`, once for `rust`), which is different from attempt 1 (where the download itself 404'd). But immediately after both downloads complete, espup logs:

```
[warn]: Installation for 'Xtensa Rust' failed, retrying. Error: HTTP GET Error: 404 Not Found
```

It then uninstalls and retries from scratch — re-downloading both tarballs each time — for 4 full cycles, then hard-fails with the same error.

**Key observation: the 404 now happens *after* successful downloads, during whatever install/extract step follows.** This means the failure has moved from "wrong asset URL" to something else — likely a second HTTP call espup makes during the install phase (e.g. a version-check/manifest/checksum fetch), not the tarball fetch itself.

### What Codex should research/try, roughly in order
1. Confirm `rustc -vV` now shows `host: aarch64-apple-darwin` (sanity check that attempt-1 fix actually landed).
2. Read `esp-rs/espup` source (`src/toolchain/mod.rs`, the `Installable` impl for Xtensa Rust) to find what HTTP GET happens *between* "All downloads complete" and the point that logs the 404 — it's not the tarball download since that already succeeded.
3. Check `esp-rs/espup` GitHub issues for reports matching "All downloads complete" immediately followed by a 404 on install (not download) — this exact sequence, not the attempt-1 symptom.
4. Try a clean-slate reinstall to rule out stale state from the two earlier partial installs:
   ```
   rm -rf ~/.rustup/toolchains/esp ~/.espup
   espup install --targets esp32s3
   ```
5. Try pinning the toolchain version explicitly (`espup install --toolchain-version 1.95.0.0 --targets esp32s3`) to rule out any "resolve latest" logic being involved.
6. Check whether an unauthenticated GitHub API rate limit is involved (espup queries `api.github.com` for release metadata) — try setting `GITHUB_TOKEN` before rerunning. Note: rate-limit responses are normally 403, not 404, so this is a lower-probability lead, but esp-rs/espup issue #495 notes espup mislabels various unrelated query failures as generic HTTP errors — worth ruling out.
7. If none of the above resolves it, consider a different espup version (`cargo install espup --locked --version <x>`) — current is 0.17.1.

## What Claude will do once this is unblocked
- Scaffold `firmware/` properly via `esp-generate -o esp32s3 -o wifi -o embassy` (current stub has no `esp-hal` deps, no `memory.x`, no target config)
- Resolve the workspace-structure decision above with the user
- Implement PIR → capture → HTTP POST state machine on firmware, Ollama/RunPod integration on server
- Run code review passes as pieces land
