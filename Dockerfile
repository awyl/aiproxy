# syntax=docker/dockerfile:1
# Pinned skills library. No package downloads during dashboard requests.
FROM docker.io/library/node:22-bookworm-slim AS skills-builder
WORKDIR /opt/aiproxy-skills
COPY skills-import/package.json skills-import/package-lock.json ./
RUN npm ci --omit=dev --ignore-scripts --engine-strict --no-audit --no-fund
COPY skills-import/helper.mjs ./

# ── Build stage: Rust binary (glibc, matches ort-sys prebuilt) ────────────────
FROM docker.io/library/debian:trixie-slim AS rust-builder

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
FROM docker.io/library/debian:trixie-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        git \
        libstdc++6 \
        libatomic1 \
        python3 \
        python3-venv \
    && rm -rf /var/lib/apt/lists/*

# Use the supported Node 22 runtime for both the helper and hosted MCP tools.
COPY --from=skills-builder /usr/local/bin/node /usr/local/bin/node
COPY --from=skills-builder /usr/local/lib/node_modules/npm /usr/local/lib/node_modules/npm
RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
    && ln -s ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx \
    && ln -s node /usr/local/bin/nodejs
COPY --from=skills-builder /opt/aiproxy-skills /usr/local/share/aiproxy/skills-import

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
