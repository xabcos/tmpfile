FROM rust:bookworm AS builder
WORKDIR /app

RUN mkdir src && echo "fn main() {}" > src/main.rs
COPY Cargo.toml .
RUN cargo build --release && rm -rf src

COPY . .
RUN touch src/main.rs && cargo build --release && mkdir -p uploads

FROM gcr.io/distroless/cc-debian12:nonroot
WORKDIR /home/nonroot
COPY --from=builder --chown=nonroot:nonroot /app/target/release/tmpfile .
COPY --from=builder --chown=nonroot:nonroot /app/uploads ./uploads
ENV PATH=/home/nonroot:$PATH
EXPOSE 8080
ENTRYPOINT ["tmpfile"]
