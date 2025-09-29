# Multi-stage build for optimal size and security
# Stage 1: Build the application
FROM rust:1.75-slim as builder

# Install required dependencies for building
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Create a new empty shell project
WORKDIR /usr/src/app

# Copy manifests
COPY Cargo.toml Cargo.lock ./

# Copy source code
COPY src ./src

# Build the application in release mode
RUN cargo build --release --bin discount-manager

# Stage 2: Create the runtime image
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Create a non-root user to run the application
RUN useradd -m -u 1000 appuser

# Copy the binary from builder
COPY --from=builder /usr/src/app/target/release/discount-manager /usr/local/bin/discount-manager

# Set ownership
RUN chown appuser:appuser /usr/local/bin/discount-manager

# Switch to non-root user
USER appuser

# Set working directory
WORKDIR /app

# Health check (optional - adjust based on your app's behavior)
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD discount-manager --help || exit 1

# The binary will be the entrypoint
ENTRYPOINT ["discount-manager"]
