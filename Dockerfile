# syntax=docker/dockerfile:1
# ==========================================================================
# Sentinel — production image (SPEC-P16 §1).
#
# Multi-stage build:
#   builder  rust:1.98-bookworm   — full release build of the Rust workspace
#   runtime  debian:bookworm-slim — ca-certificates + curl, non-root `sentinel`
#
# WORKDIR /app mirrors the repo layout so replay works verbatim inside the
# image:  sentinel --mode dry-run --replay tests/fixtures/perpl/<fixture>.jsonl
#
# Toolchain note (changelog): the P16 prompt originally pinned `rust:1.85`;
# that is superseded by the workspace's always-latest rule (rust-toolchain.toml
# tracks latest stable — rustc 1.98.1 at build time). This image pins
# `rust:1.98-bookworm` for parity with the host toolchain. `rust-toolchain.toml`
# is intentionally NOT copied into the builder: `channel = "stable"` would make
# rustup fetch whatever stable is newest *at build time*; the image's own 1.98
# toolchain is the deterministic choice.
#
# The image never contains `.env` or any runtime secret — configuration is
# injected at run time (compose `env_file .env`, Railway service variables).
# ==========================================================================

# ---- Stage 1: builder ----------------------------------------------------
FROM rust:1.98-bookworm AS builder

WORKDIR /build

# Manifests first, then workspace sources (single workspace; dependency
# sources come from crates.io — nothing vendored is required).
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# Build-time embed: crates/sentinel/src/api.rs uses
# include_str!("../../../dashboard/index.html") for the `GET /` dashboard.
COPY dashboard ./dashboard

# --locked pins the build to Cargo.lock; --workspace builds the daemon, all
# of its bins (read-positions, test-execution, brain_eval, test-nansen,
# audit-verify, backtest) and the breaker.
RUN cargo build --release --locked --workspace

# ---- Stage 2: runtime ----------------------------------------------------
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

# Non-root runtime user. Defaults to 10001:10001; pass
#   --build-arg SENTINEL_UID=$(id -u) --build-arg SENTINEL_GID=$(id -g)
# when bind-mounting a host directory (compose ./data:/app/data) so the
# daemon can write the audit journal — see scripts/docker-verify.sh and
# docs/RUNBOOK.md.
ARG SENTINEL_UID=10001
ARG SENTINEL_GID=10001
RUN groupadd --system --gid "${SENTINEL_GID}" sentinel \
    && useradd --system --uid "${SENTINEL_UID}" --gid "${SENTINEL_GID}" \
       --create-home --home-dir /home/sentinel --shell /usr/sbin/nologin sentinel

WORKDIR /app

# Workspace binaries (daemon + its bins + breaker).
COPY --from=builder /build/target/release/sentinel /usr/local/bin/sentinel
COPY --from=builder /build/target/release/read-positions /usr/local/bin/read-positions
COPY --from=builder /build/target/release/test-execution /usr/local/bin/test-execution
COPY --from=builder /build/target/release/brain_eval /usr/local/bin/brain_eval
COPY --from=builder /build/target/release/test-nansen /usr/local/bin/test-nansen
COPY --from=builder /build/target/release/audit-verify /usr/local/bin/audit-verify
COPY --from=builder /build/target/release/backtest /usr/local/bin/backtest
COPY --from=builder /build/target/release/breaker /usr/local/bin/breaker

# Repo-layout assets the runtime reads:
# - dashboard/                — dashboard surface (P15)
# - docs/backtest-report.json — dashboard backtest panel default path
# - tests/fixtures/perpl/     — scenario fixtures used by `--replay`
# (contracts/out is deliberately not copied — the daemon does not need it.)
COPY dashboard ./dashboard
COPY docs/backtest-report.json ./docs/backtest-report.json
COPY tests/fixtures/perpl ./tests/fixtures/perpl

# Runtime state directories (the compose volume mounts ./data over /app/data).
RUN mkdir -p /app/data /app/logs \
    && chown -R sentinel:sentinel /app

ENV PORT=8080
EXPOSE 8080

USER sentinel

HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD curl -f http://localhost:8080/healthz || exit 1

CMD ["/usr/local/bin/sentinel"]
