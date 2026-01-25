# Stage 1: Builder
# Use the official Rust image (using slim-bookworm for a smaller initial footprint that matches runtime)
FROM rust:1.91 as builder

WORKDIR /usr/src/app

# Install build dependencies required by barter-data/openssl-sys
RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

# Copy the entire workspace context
# (We assume .dockerignore excludes target/, .git/, etc.)
COPY . .

# Build the specific backtest example in release mode
# The binary will be located at target/release/examples/bond_event_signals_backtest
RUN cargo build --release --example bond_event_signals_backtest -p barter

# Stage 2: Runtime
# Use a lightweight Debian image for runtime
FROM debian:bookworm-slim

WORKDIR /app

# Install runtime dependencies (OpenSSL is required for network/crypto ops in barter)
# ca-certificates is good practice if any HTTPS calls happen.
RUN apt-get update && apt-get install -y libssl3 ca-certificates && rm -rf /var/lib/apt/lists/*

# Copy the compiled binary from the builder stage
COPY --from=builder /usr/src/app/target/release/examples/bond_event_signals_backtest /usr/local/bin/backtester

# Create a generic data directory where we expect mounts
RUN mkdir -p /app/data

# Set the entrypoint to the binary
ENTRYPOINT ["backtester"]

# Default arguments which can be overridden by K8s args or Docker command
# We point to a default location where PVCs would mount data
CMD ["/app/data/bond_marks.csv", "/app/data/bond_signals.csv"]
