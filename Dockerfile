# syntax=docker/dockerfile:1
# ── Build stage: Rust binary (glibc, matches ort-sys prebuilt) ────────────────
FROM debian:trixie-slim AS rust-builder

RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential curl pkg-config ca-certificates && rm -rf /var/lib/apt/lists/*

# Install Rust via rustup (trixie has glibc 2.40, needed by ort-sys prebuilt)
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"
RUN cargo --version && rustc --version

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/root/.cargo/git \
    --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,sharing=shared,target=/app/target \
    cargo build --release && cp /app/target/release/aiproxy /usr/local/bin/aiproxy

# ── Runtime stage (Debian, glibc for onnxruntime) ─────────────────────────────
FROM debian:trixie-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        nodejs \
        npm \
        python3 \
        python3-venv \
    && rm -rf /var/lib/apt/lists/*

# uv (for uvx)
RUN curl -LsSf https://astral.sh/uv/install.sh | sh
ENV PATH="/root/.local/bin:${PATH}"

# Copy binary from build stage
COPY --from=rust-builder /usr/local/bin/aiproxy /usr/local/bin/aiproxy

RUN mkdir -p /etc/aiproxy /models /runtime
VOLUME ["/etc/aiproxy", "/models", "/runtime"]
ENV FASTEMBED_CACHE_DIR=/models
EXPOSE 8080

ENTRYPOINT ["aiproxy"]
CMD ["--config", "/etc/aiproxy/aiproxy.yaml"]
