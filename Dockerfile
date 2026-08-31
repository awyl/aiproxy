# ── Build stage ──────────────────────────────────────────────────────────────
FROM rust:1.87-alpine AS builder

RUN apk add --no-cache musl-dev

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
# Cache deps: create dummy src, build deps only, then copy real src
RUN mkdir src && echo 'fn main() {}' > src/main.rs && echo 'fn main() {}' > src/lib.rs
RUN cargo build --release 2>/dev/null || true
RUN rm -rf src

COPY src ./src
RUN touch src/main.rs && cargo build --release

# ── Runtime stage ────────────────────────────────────────────────────────────
FROM alpine:3.21

# Runtime deps: Node.js (npx), Python (uvx), curl (uv install)
RUN apk add --no-cache \
        nodejs \
        npm \
        python3 \
        py3-pip \
        curl

# uv (for uvx)
RUN curl -LsSf https://astral.sh/uv/install.sh | sh
ENV PATH="/root/.local/bin:${PATH}"

# Copy the statically linked binary
COPY --from=builder /app/target/release/aiproxy /usr/local/bin/aiproxy

RUN mkdir -p /etc/aiproxy
VOLUME ["/etc/aiproxy"]

EXPOSE 8080

ENTRYPOINT ["aiproxy"]
CMD ["--config", "/etc/aiproxy/aiproxy.yaml"]
