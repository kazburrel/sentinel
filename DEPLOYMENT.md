# Deployment and Release Checks

## Purpose

This project has two separately deployed programs:

- **Firmware** is compiled on the Mac and flashed onto the ESP32-S3 camera board.
- **Server** runs off-board. RunPod is the temporary hosted target; a future Mac mini
  or other mini computer will become the permanent local target.

GitHub is optional source control and CI. The application does not run on GitHub.
Tests only block a release when the deployment workflow is configured to require
them; merely having tests in the repository is not enough.

## Current and planned topology

Current proven development path:

`ESP32 -> home WiFi -> Mac trusted-LAN receiver`

Temporary hosted target:

`ESP32 -> HTTPS -> authenticated RunPod endpoint -> persistent storage/AI`

Future local target:

`ESP32 -> isolated IoT network -> Mac mini server -> local storage/AI`

The current Rust receiver listens with plain HTTP and a project upload token. It is a
trusted-LAN prototype and must **not** be published directly on RunPod or exposed via
router port forwarding. RunPod endpoints are internet-facing and require HTTPS,
public-endpoint authentication, tighter abuse controls, and persistent storage.

## RunPod decision and constraints

RunPod is the temporary server platform until the mini computer is available. The
exact RunPod product and upload architecture must be chosen before deployment:

- A **RunPod Pod HTTP service** can host a conventional long-running server. Its HTTP
  proxy provides an HTTPS public URL, but the endpoint is publicly reachable and must
  authenticate every request. The proxy has a 100-second request limit.
- **RunPod Serverless** runs containerized handlers and can scale to zero, but its
  queue endpoints use RunPod's API request/response format rather than the current raw
  `POST /upload` protocol. Large media should be stored persistently and passed to AI
  by reference rather than treated as permanent worker-local files.
- RunPod container disks are ephemeral. Recordings that must survive worker or Pod
  replacement must live on a RunPod network volume or suitable S3-compatible object
  storage. Never acknowledge permanent receipt merely because a file reached an
  ephemeral container directory.
- The ESP32 currently posts plain HTTP to a numeric LAN address. Direct RunPod upload
  additionally requires DNS/hostname handling, TLS certificate validation, the final
  RunPod request format, and a narrowly scoped deployment credential. Do not put a
  full-account RunPod API key in firmware.

Until that hosted path is implemented and verified, the Mac receiver remains the
working ingestion path. RunPod can still be used for AI after the Mac forwards an
event; that bridge is safer and simpler than prematurely exposing the current server.

## Mandatory release gates

Every deployment must stop immediately if its relevant checks fail.

### Shared and server gate

Run from the repository root:

```sh
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build --release -p server
```

This gate protects code deployed to RunPod and later to the mini computer. In
particular, the server unit tests protect header limits and other parsing boundaries.
A failing test must block container creation, upload, service restart, and production
promotion.

Before a real-media release, also verify:

- Valid credentials succeed; missing/incorrect credentials and unknown routes return
  the same generic `404` response.
- Exact-limit headers pass; oversized and unterminated headers fail; coalesced body
  bytes do not count toward the header limit.
- Oversized and truncated bodies fail without being acknowledged or retained as a
  successful event.
- Collision-safe filenames preserve every accepted upload.
- Client-provided paths cannot influence storage locations.
- Secrets are absent from `git status`, logs, binaries intended for public sharing,
  container layers, and example configuration files.

### Firmware gate

Run from `firmware/` with the Espressif Rust environment loaded:

```sh
cargo fmt --all -- --check
cargo build --release --bin firmware
cargo clippy --release --bin firmware -- -D warnings
```

Only flash after all three pass. Firmware tests/build checks protect what reaches the
camera board; server unit tests should not unnecessarily block a camera-only diagnostic
binary, but the integrated firmware release requires both firmware and server gates.

Before flashing integrated WiFi/upload firmware, verify:

- The target board and serial port are correct.
- Real WiFi, upload, and RunPod credentials remain in ignored local/secret storage.
- `RECORDING_ENABLED` has the intended release value.
- Server hostname, port, TLS mode, certificate validation, payload limit, and protocol
  version match the deployed endpoint.
- USB export remains available as the documented debugging/fallback path.

### RunPod container gate

Before publishing a RunPod image:

- Use a reproducible Dockerfile and a pinned Rust/toolchain/dependency build.
- Build for `linux/amd64` when required by the chosen RunPod deployment target; this
  matters because the development Mac is Apple Silicon.
- Run the shared/server gate inside the build workflow before producing the final
  image.
- Run the image locally and execute health, valid-upload, invalid-auth, oversized,
  truncated-upload, persistence, and restart tests.
- Use immutable version or commit-SHA image tags. Do not deploy an untraceable
  floating `latest` image as the only production reference.
- Inject secrets at runtime through RunPod secret/environment configuration; never
  bake them into the image.
- Attach and verify persistent storage before accepting real recordings.
- Confirm uploaded events survive a worker/container restart and that concurrent
  uploads do not corrupt or overwrite one another.
- Confirm the public endpoint uses HTTPS and no unintended ports or admin services are
  exposed.
- Set cost limits, worker scaling limits, request/body limits, timeouts, logs,
  monitoring, and a retention/deletion policy.

## Deployment procedure

1. Make the intended change in a small reviewable step.
2. Review `git diff` and confirm no credentials or unrelated files are included.
3. Run the relevant mandatory release gates above.
4. Build an immutable release artifact:
   - firmware binary for the ESP32, or
   - versioned container image for RunPod/server deployment.
5. Deploy first to a test/staging endpoint or local test instance.
6. Run smoke tests and negative security tests.
7. Promote the exact same artifact; do not rebuild different source for production.
8. Record the commit, artifact/image identifier, configuration version, deployment
   time, and test results.
9. Keep the previous known-good artifact available for rollback.

## Failure and rollback rules

- A failed test, build, lint, security check, persistence test, or health check stops
  deployment. Do not bypass it merely because the code compiles.
- A failed firmware release means do not flash it; keep the last verified firmware on
  the board.
- A failed RunPod release means keep/restore the previous image and do not direct the
  camera to the new endpoint.
- Never delete the ESP32/SD fallback copy until the server has returned a truthful
  acknowledgement that the complete event is durably stored.
- Rotate deployment/upload credentials immediately if they appear in git, terminal
  output shared publicly, logs, screenshots, or a published container layer.

## GitHub and automation

GitHub is not required to run the project. If GitHub is used, configure CI and branch
protection so a pull request cannot merge until formatting, tests, clippy, builds, and
secret scanning pass. A deployment workflow should consume only an approved commit
and should refuse to publish when any required job fails.

For local-only work, provide a deployment script that runs the same gates with shell
error handling enabled before flashing or publishing. This gives the same fail-closed
behavior without depending on GitHub.

## Migration from RunPod to the mini computer

The shared event envelope must remain independent of RunPod. When the mini computer
arrives:

1. Build and test the same server for the mini computer's CPU/OS.
2. Configure persistent local storage, backups, retention, firewall, and monitoring.
3. Put the camera on an IoT VLAN/SSID that can reach only the server upload port.
4. Test a full event, acknowledgement, retry, restart, and storage-recovery cycle.
5. Change the camera endpoint configuration, not the camera/event format.
6. Keep RunPod only for AI jobs if desired, or retire it after data migration and key
   revocation are complete.

## Deferred decisions that must be resolved before RunPod production

- RunPod Pod versus Serverless endpoint.
- Whether the ESP32 uploads directly to RunPod or the Mac temporarily forwards events.
- Persistent network volume versus external S3-compatible object storage.
- TLS implementation and certificate validation on the ESP32.
- Restricted device credential format, rotation, revocation, and rate limiting.
- Event retention, deletion, backup, privacy, and cost limits.

