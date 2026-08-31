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

# ── Build stage: llama.cpp (Debian glibc, GGML_NATIVE for CPU optimizations) ─
FROM debian:trixie-slim AS llama-builder

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates git cmake g++ make \
    && rm -rf /var/lib/apt/lists/*

RUN git clone --depth 1 https://github.com/ggml-org/llama.cpp.git /llama.cpp

WORKDIR /llama.cpp
RUN cmake -B build \
        -DCMAKE_BUILD_TYPE=Release \
        -DGGML_NATIVE=ON \
        -DLLAMA_CURL=OFF \
    && cmake --build build --config Release -j$(nproc) \
    && cp build/bin/llama-server /usr/local/bin/llama-server

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
COPY --from=llama-builder /usr/local/bin/llama-server /usr/local/bin/llama-server

RUN mkdir -p /etc/aiproxy /models
VOLUME ["/etc/aiproxy", "/models"]

EXPOSE 8080

ENTRYPOINT ["aiproxy"]
CMD ["--config", "/etc/aiproxy/aiproxy.yaml"]
