# Build stage
FROM rust:bookworm as builder

# Install musl-tools for static linking
RUN apt-get update && apt-get install -y musl-tools && \
    rustup target add x86_64-unknown-linux-musl

WORKDIR /build

# Copy manifests
COPY Cargo.toml Cargo.lock ./

# Build dependencies - this is the caching layer
RUN mkdir src && \
    echo "fn main() {}" > src/main.rs && \
    cargo build --target x86_64-unknown-linux-musl --release && \
    rm -rf src target/x86_64-unknown-linux-musl/release/deps/file_server*

# Copy source code
COPY src ./src

# Build application
RUN cargo build --target x86_64-unknown-linux-musl --release && \
    cp target/x86_64-unknown-linux-musl/release/file-server /build/tmpfile && \
    strip /build/tmpfile

# Create uploads directory
RUN mkdir -p /home/nonroot/uploads

# Final stage
FROM gcr.io/distroless/static-debian12:nonroot
WORKDIR /home/nonroot
ENV PATH=/home/nonroot:$PATH

# Set default environment variables
ENV LISTEN_ADDR=0.0.0.0:8080 \
    UPLOAD_DIR=/home/nonroot/uploads \
    MAX_FILE_SIZE=268435456 \
    FILE_EXPIRATION=129600 \
    MAX_DOWNLOAD_COUNT=0

# Copy uploads directory and binary
COPY --from=builder --chown=nonroot:nonroot /home/nonroot/uploads /build/tmpfile .

USER nonroot
EXPOSE 8080
ENTRYPOINT ["tmpfile"]
