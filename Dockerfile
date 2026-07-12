# syntax=docker/dockerfile:1.7
FROM rust:bookworm AS builder
WORKDIR /app

COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/app/target,sharing=locked \
    cargo test --locked --all-targets --all-features \
    && cargo build --locked --release \
    && install -m 755 target/release/tmpfile /tmp/tmpfile \
    && mkdir -p /tmp/uploads

FROM gcr.io/distroless/cc-debian13:nonroot
WORKDIR /home/nonroot
COPY --from=builder --chown=nonroot:nonroot /tmp/tmpfile .
COPY --from=builder --chown=nonroot:nonroot /tmp/uploads ./uploads
ENV PATH=/home/nonroot:$PATH
EXPOSE 8080
ENTRYPOINT ["tmpfile"]
