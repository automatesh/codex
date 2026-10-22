# Codex stripped

Fork of `openai/codex`, pinned to `rust-v0.163.0`
(`83086188ae0614bd882818ff1aa23f3194962012`).

This branch removes the client's outbound analytics and observability transports.
It preserves the upstream coding and research engine: TUI, `exec`, shell and patch
tools, MCP, skills, subagents, hooks, session persistence, resume, and compaction.

## Removed

- Product analytics queue, background worker, HTTP delivery, and debug capture.
- OpenTelemetry API, SDK, semantic conventions, tracing integration and OTLP exporters.
- Guardian V2 action classifier and synchronous reviewer, their extension hooks,
  review sessions, retry loops, rejection circuit breaker and decision cache.
- OTEL exporter initialization, runtime OTEL event emission, and automatic HTTP/MCP trace propagation.
- Startup metric buffering before a provider is installed.
- Feedback request/auth metadata collection and its tracing layer.
- Sentry feedback uploads, envelope construction, attachments for upload, and the
  Sentry dependency. Feedback submission returns a disabled error.

`analytics.enabled`, `feedback.enabled`, and explicit OTLP exporter settings cannot
enable these transports. Config loading exposes analytics and feedback as disabled.

## Compatibility retained

The `codex-analytics` and `codex-otel` public interfaces remain so coding features do
not need wholesale rewrites. Analytics calls discard their input. OTEL provider
creation returns `None`. Local counters, histograms and metric snapshots use plain
Rust types; they do not depend on OpenTelemetry and have no network exporter or
background worker.

Local diagnostic logs and session files are retained. Model requests, authentication,
web research, MCP, and connected tools still require their normal network access.
Manual approval and sandbox enforcement remain. Guardian is unavailable; explicit
AutoReview settings and managed requirements that mandate it are rejected rather
than silently ignored. Protocol/history fields remain readable for old sessions.
Handling of server-side policy errors, including `cyber_policy`, is unchanged.

## Build

Use the pinned Rust toolchain in `codex-rs/rust-toolchain.toml` (Rust 1.95.0).
The remaining prerequisites are documented in [docs/install.md](docs/install.md).

```sh
cd codex-rs
cargo build --locked -p codex-cli --bin codex
```

The executable is written to `codex-rs/target/debug/codex` (`codex.exe` on Windows).
Building does not install it or change an existing Codex installation.

## Checks

```sh
cd codex-rs
cargo check --locked -p codex-cli
cargo test --locked -p codex-analytics -p codex-feedback -p codex-otel -- --test-threads=1
cargo test --locked -p codex-http-client -- --test-threads=1
cargo test --locked -p codex-core --test stripped_config -- --test-threads=1
```

Transport regression tests cover explicit exporter configuration, analytics opt-in,
feedback submission, and automatic trace headers. Tests for removed upload/export
implementations are replaced by disabled-transport contracts; local diagnostic and
metric tests are retained.
