# ── Build stage: Rust binary (static musl) ───────────────────────────────────
FROM rust:1.96-alpine AS rust-builder

RUN apk add --no-cache musl-dev

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && echo 'fn main() {}' > src/lib.rs
RUN cargo build --release 2>/dev/null || true
RUN rm -rf src

COPY src ./src
RUN touch src/main.rs && cargo build --release
# ── Runtime stage (Debian, glibc for llama.cpp native perf) ──────────────────
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

# Copy binaries from build stages
COPY --from=rust-builder /app/target/release/aiproxy /usr/local/bin/aiproxy

RUN mkdir -p /etc/aiproxy
VOLUME ["/etc/aiproxy"]

EXPOSE 8080

ENTRYPOINT ["aiproxy"]
CMD ["--config", "/etc/aiproxy/aiproxy.yaml"]
