APP=tmpfile
IMAGE=proxyxai/tmpfile
VERSION?=rust
SANITIZED_VERSION:=$(subst :,-,$(VERSION))

.PHONY: build run clean docker-build docker-run fmt

build:
	cargo build --release

run:
	cargo run

clean:
	cargo clean

fmt:
	cargo fmt --all || true

docker-build:
	docker build -t $(IMAGE):$(SANITIZED_VERSION) .
	docker push $(IMAGE):$(SANITIZED_VERSION)

docker-run:
	docker run -p 8080:8080 $(IMAGE):$(SANITIZED_VERSION)
