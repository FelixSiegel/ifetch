FROM rust:slim-bookworm AS builder

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        pkg-config \
        libssl-dev && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /usr/src/ifetch

# Normally docker would invalidate the cache after each code change. To speed up compile time,
# we first only copy over the rust configs, create a dummy project so docker pulls all dependencies
# as this step never changes, it will kept on subsequent runs
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && \
    echo "fn main() {}" > src/main.rs && \
    cargo build --release && \
    rm -Rvf src

# Now we copy the actual code and compile it as a new step
COPY src ./src
RUN touch src/main.rs && cargo build --release

# We use googles minimal distroless runtime but the cc variant as we require ssl
FROM gcr.io/distroless/cc-debian12

WORKDIR /app
COPY --from=builder /usr/src/ifetch/target/release/ifetch /usr/local/bin/ifetch

# Default env vars for docker usage
ENV IFETCH_SERVER=true
ENV IFETCH_PORT=8080
ENV IFETCH_OUTPUT=/app/downloads
ENV IFETCH_CONFIG=/app/config
ENV RUST_LOG=info

EXPOSE 8080

CMD ["ifetch"]
