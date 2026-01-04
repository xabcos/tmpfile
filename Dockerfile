FROM rust:bookworm AS builder
WORKDIR /app

COPY . .
RUN cargo build --release && mkdir -p uploads

FROM gcr.io/distroless/cc-debian12:nonroot
WORKDIR /home/nonroot
COPY --from=builder --chown=nonroot:nonroot /app/target/release/tmpfile .
COPY --from=builder --chown=nonroot:nonroot /app/uploads ./uploads
ENV PATH=/home/nonroot:$PATH
EXPOSE 8080
ENTRYPOINT ["tmpfile"]
