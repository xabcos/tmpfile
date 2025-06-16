# Project configuration
PROJECT_NAME = tmpfile-rust
BINARY_NAME = tmpfile
DOCKER_IMAGE = tmpfile-rust
DOCKER_TAG ?= latest
REGISTRY ?=

# Build configuration
RUST_TARGET = x86_64-unknown-linux-musl
CARGO_FLAGS = --release
BUILD_DIR = target/$(RUST_TARGET)/release
LOCAL_BUILD_DIR = target/release

# Runtime configuration
LISTEN_ADDR ?= 0.0.0.0:8080
UPLOAD_DIR ?= ./uploads
MAX_FILE_SIZE ?= 268435456
FILE_EXPIRATION ?= 129600
MAX_DOWNLOAD_COUNT ?= 0
API_KEY ?=
REDIS_ADDR ?=
REDIS_DB ?= 0

# Colors for pretty output
GREEN = \033[0;32m
YELLOW = \033[0;33m
RED = \033[0;31m
BLUE = \033[0;34m
NC = \033[0m # No Color

# Check if commands exist
DOCKER_EXISTS := $(shell command -v docker 2> /dev/null)
CARGO_WATCH_EXISTS := $(shell command -v cargo-watch 2> /dev/null)

.PHONY: all build clean test run dev docker-build docker-run docker-push help install-deps check fmt clippy

# Default target
all: build

## Build commands
build: install-target ## Build the application for production
	@echo "$(GREEN)Building $(PROJECT_NAME)...$(NC)"
	@cargo build $(CARGO_FLAGS) --target $(RUST_TARGET)
	@if [ -f "$(BUILD_DIR)/$(BINARY_NAME)" ]; then \
		echo "$(GREEN)Build complete: $(BUILD_DIR)/$(BINARY_NAME)$(NC)"; \
		ls -lh $(BUILD_DIR)/$(BINARY_NAME); \
	else \
		echo "$(RED)Build failed!$(NC)"; \
		exit 1; \
	fi

build-local: ## Build the application for local development
	@echo "$(GREEN)Building $(PROJECT_NAME) for local development...$(NC)"
	@cargo build $(CARGO_FLAGS)
	@if [ -f "$(LOCAL_BUILD_DIR)/$(BINARY_NAME)" ]; then \
		echo "$(GREEN)Build complete: $(LOCAL_BUILD_DIR)/$(BINARY_NAME)$(NC)"; \
		ls -lh $(LOCAL_BUILD_DIR)/$(BINARY_NAME); \
	else \
		echo "$(RED)Build failed!$(NC)"; \
		exit 1; \
	fi

dev: ## Build and run in development mode with auto-reload
ifndef CARGO_WATCH_EXISTS
	@echo "$(YELLOW)cargo-watch not found. Installing...$(NC)"
	@cargo install cargo-watch
endif
	@echo "$(GREEN)Starting development server with auto-reload...$(NC)"
	@echo "$(BLUE)Press Ctrl+C to stop$(NC)"
	@mkdir -p $(UPLOAD_DIR)
	@LISTEN_ADDR=$(LISTEN_ADDR) \
	UPLOAD_DIR=$(UPLOAD_DIR) \
	MAX_FILE_SIZE=$(MAX_FILE_SIZE) \
	FILE_EXPIRATION=$(FILE_EXPIRATION) \
	MAX_DOWNLOAD_COUNT=$(MAX_DOWNLOAD_COUNT) \
	API_KEY=$(API_KEY) \
	REDIS_ADDR=$(REDIS_ADDR) \
	REDIS_DB=$(REDIS_DB) \
	cargo watch -x run

run: build-local ## Run the application locally
	@echo "$(GREEN)Starting $(PROJECT_NAME)...$(NC)"
	@mkdir -p $(UPLOAD_DIR)
	@echo "$(BLUE)Server will be available at http://$(LISTEN_ADDR)$(NC)"
	@echo "$(BLUE)Upload directory: $(UPLOAD_DIR)$(NC)"
	@echo "$(BLUE)Press Ctrl+C to stop$(NC)"
	@LISTEN_ADDR=$(LISTEN_ADDR) \
	UPLOAD_DIR=$(UPLOAD_DIR) \
	MAX_FILE_SIZE=$(MAX_FILE_SIZE) \
	FILE_EXPIRATION=$(FILE_EXPIRATION) \
	MAX_DOWNLOAD_COUNT=$(MAX_DOWNLOAD_COUNT) \
	API_KEY=$(API_KEY) \
	REDIS_ADDR=$(REDIS_ADDR) \
	REDIS_DB=$(REDIS_DB) \
	./$(LOCAL_BUILD_DIR)/$(BINARY_NAME)

run-with-redis: build-local ## Run with Redis backend
	@echo "$(GREEN)Starting $(PROJECT_NAME) with Redis...$(NC)"
	@echo "$(BLUE)Checking Redis connection...$(NC)"
	@redis-cli -h 127.0.0.1 -p 6379 ping > /dev/null 2>&1 || \
		(echo "$(YELLOW)Warning: Cannot connect to Redis at 127.0.0.1:6379$(NC)"; \
		 echo "$(YELLOW)Make sure Redis is running or the server will use file-based storage$(NC)")
	@mkdir -p $(UPLOAD_DIR)
	@LISTEN_ADDR=$(LISTEN_ADDR) \
	UPLOAD_DIR=$(UPLOAD_DIR) \
	MAX_FILE_SIZE=$(MAX_FILE_SIZE) \
	FILE_EXPIRATION=$(FILE_EXPIRATION) \
	MAX_DOWNLOAD_COUNT=$(MAX_DOWNLOAD_COUNT) \
	API_KEY=$(API_KEY) \
	REDIS_ADDR=127.0.0.1:6379 \
	REDIS_DB=$(REDIS_DB) \
	./$(LOCAL_BUILD_DIR)/$(BINARY_NAME)

## Development commands
test: ## Run all tests
	@echo "$(GREEN)Running tests...$(NC)"
	@cargo test --all-features

test-verbose: ## Run tests with verbose output
	@echo "$(GREEN)Running tests (verbose)...$(NC)"
	@cargo test --all-features -- --nocapture

check: ## Check code without building
	@echo "$(GREEN)Checking code...$(NC)"
	@cargo check --all-targets --all-features

fmt: ## Format code
	@echo "$(GREEN)Formatting code...$(NC)"
	@cargo fmt

fmt-check: ## Check code formatting
	@echo "$(GREEN)Checking code formatting...$(NC)"
	@cargo fmt -- --check

clippy: ## Run clippy linter
	@echo "$(GREEN)Running clippy...$(NC)"
	@cargo clippy --all-targets --all-features -- -D warnings

clippy-fix: ## Fix clippy warnings automatically
	@echo "$(GREEN)Fixing clippy warnings...$(NC)"
	@cargo clippy --fix --all-targets --all-features

audit: ## Security audit
	@echo "$(GREEN)Running security audit...$(NC)"
	@cargo audit

pre-commit: fmt clippy test ## Run all pre-commit checks
	@echo "$(GREEN)All pre-commit checks passed!$(NC)"

## Docker commands
docker-build: ## Build Docker image
ifndef DOCKER_EXISTS
	@echo "$(RED)Error: Docker not found$(NC)"
	@exit 1
endif
	@echo "$(GREEN)Building Docker image...$(NC)"
	@docker build -t $(DOCKER_IMAGE):$(DOCKER_TAG) .
	@echo "$(GREEN)Docker image built: $(DOCKER_IMAGE):$(DOCKER_TAG)$(NC)"
	@docker images | grep $(DOCKER_IMAGE)

docker-build-no-cache: ## Build Docker image without cache
ifndef DOCKER_EXISTS
	@echo "$(RED)Error: Docker not found$(NC)"
	@exit 1
endif
	@echo "$(GREEN)Building Docker image (no cache)...$(NC)"
	@docker build --no-cache -t $(DOCKER_IMAGE):$(DOCKER_TAG) .

docker-run: docker-build ## Run Docker container
	@echo "$(GREEN)Running Docker container...$(NC)"
	@echo "$(BLUE)Server will be available at http://localhost:8080$(NC)"
	@mkdir -p $(PWD)/uploads
	@docker run --rm -it \
		-p 8080:8080 \
		-v $(PWD)/uploads:/home/nonroot/uploads \
		-e LISTEN_ADDR=0.0.0.0:8080 \
		-e MAX_FILE_SIZE=$(MAX_FILE_SIZE) \
		-e FILE_EXPIRATION=$(FILE_EXPIRATION) \
		-e MAX_DOWNLOAD_COUNT=$(MAX_DOWNLOAD_COUNT) \
		$(if $(API_KEY),-e API_KEY=$(API_KEY),) \
		$(if $(REDIS_ADDR),-e REDIS_ADDR=$(REDIS_ADDR),) \
		$(if $(REDIS_DB),-e REDIS_DB=$(REDIS_DB),) \
		$(DOCKER_IMAGE):$(DOCKER_TAG)

docker-run-daemon: docker-build ## Run Docker container in background
	@echo "$(GREEN)Running Docker container in background...$(NC)"
	@mkdir -p $(PWD)/uploads
	@docker run -d \
		--name $(PROJECT_NAME) \
		-p 8080:8080 \
		-v $(PWD)/uploads:/home/nonroot/uploads \
		-e LISTEN_ADDR=0.0.0.0:8080 \
		-e MAX_FILE_SIZE=$(MAX_FILE_SIZE) \
		-e FILE_EXPIRATION=$(FILE_EXPIRATION) \
		-e MAX_DOWNLOAD_COUNT=$(MAX_DOWNLOAD_COUNT) \
		$(if $(API_KEY),-e API_KEY=$(API_KEY),) \
		$(if $(REDIS_ADDR),-e REDIS_ADDR=$(REDIS_ADDR),) \
		$(if $(REDIS_DB),-e REDIS_DB=$(REDIS_DB),) \
		$(DOCKER_IMAGE):$(DOCKER_TAG)
	@echo "$(GREEN)Container started: $(PROJECT_NAME)$(NC)"
	@echo "$(BLUE)Server available at http://localhost:8080$(NC)"
	@echo "$(BLUE)Use 'make docker-logs' to view logs$(NC)"

docker-stop: ## Stop Docker container
	@echo "$(YELLOW)Stopping Docker container...$(NC)"
	@docker stop $(PROJECT_NAME) 2>/dev/null || echo "$(YELLOW)Container not running$(NC)"
	@docker rm $(PROJECT_NAME) 2>/dev/null || echo "$(YELLOW)Container not found$(NC)"

docker-logs: ## Show Docker container logs
	@docker logs -f $(PROJECT_NAME) 2>/dev/null || echo "$(RED)Container not running$(NC)"

docker-status: ## Show Docker container status
	@echo "$(GREEN)Container status:$(NC)"
	@docker ps -a | grep $(PROJECT_NAME) || echo "$(YELLOW)No containers found$(NC)"

docker-push: ## Push Docker image to registry
ifndef REGISTRY
	@echo "$(RED)Error: REGISTRY not set$(NC)"
	@echo "Usage: make docker-push REGISTRY=your-registry.com"
	@exit 1
endif
	@echo "$(GREEN)Pushing to registry...$(NC)"
	@docker tag $(DOCKER_IMAGE):$(DOCKER_TAG) $(REGISTRY)/$(DOCKER_IMAGE):$(DOCKER_TAG)
	@docker push $(REGISTRY)/$(DOCKER_IMAGE):$(DOCKER_TAG)
	@echo "$(GREEN)Pushed: $(REGISTRY)/$(DOCKER_IMAGE):$(DOCKER_TAG)$(NC)"

## Utility commands
install-deps: ## Install development dependencies
	@echo "$(GREEN)Installing development dependencies...$(NC)"
	@cargo install cargo-watch cargo-audit 2>/dev/null || echo "$(YELLOW)Some tools may already be installed$(NC)"

install-target: ## Install musl target
	@echo "$(GREEN)Installing musl target...$(NC)"
	@rustup target add $(RUST_TARGET)

clean: ## Clean build artifacts
	@echo "$(YELLOW)Cleaning build artifacts...$(NC)"
	@cargo clean
	@echo "$(YELLOW)Cleaning upload directory...$(NC)"
	@rm -rf uploads/* 2>/dev/null || true

clean-all: clean docker-clean ## Clean everything including Docker
	@echo "$(YELLOW)Cleaning everything...$(NC)"

docker-clean: ## Clean Docker images and containers
	@echo "$(YELLOW)Cleaning Docker artifacts...$(NC)"
	@docker stop $(PROJECT_NAME) 2>/dev/null || true
	@docker rm $(PROJECT_NAME) 2>/dev/null || true
	@docker rmi $(DOCKER_IMAGE):$(DOCKER_TAG) 2>/dev/null || true

size: build ## Show binary size
	@echo "$(GREEN)Binary sizes:$(NC)"
	@echo "Production binary (musl):"
	@ls -lh $(BUILD_DIR)/$(BINARY_NAME) 2>/dev/null || echo "$(RED)Production binary not found. Run 'make build' first.$(NC)"
	@if [ -f "$(LOCAL_BUILD_DIR)/$(BINARY_NAME)" ]; then \
		echo "Local binary:"; \
		ls -lh $(LOCAL_BUILD_DIR)/$(BINARY_NAME); \
	fi

## Testing commands
test-upload: ## Test file upload (requires running server)
	@echo "$(GREEN)Testing file upload...$(NC)"
	@echo "This is a test file created at $$(date)" > test-upload.txt
	@echo "$(BLUE)Uploading test file...$(NC)"
	@curl -f -F "file=@test-upload.txt" http://localhost:8080/upload && \
		echo "$(GREEN)Upload successful!$(NC)" || \
		echo "$(RED)Upload failed! Make sure server is running on localhost:8080$(NC)"
	@rm -f test-upload.txt

test-api: ## Test API endpoints
	@echo "$(GREEN)Testing API endpoints...$(NC)"
	@echo "$(BLUE)Testing health check...$(NC)"
	@curl -f http://localhost:8080/health && echo "" || echo "$(RED)Health check failed$(NC)"
	@echo "$(BLUE)Testing upload...$(NC)"
	@echo "API test file" > api-test.txt
	@curl -f -F "file=@api-test.txt" http://localhost:8080/upload || echo "$(RED)Upload test failed$(NC)"
	@rm -f api-test.txt

benchmark: ## Run basic benchmarks
	@echo "$(GREEN)Running benchmarks...$(NC)"
	@cargo bench

## Quick start commands
quick-start: build-local ## Quick start for development
	@echo "$(GREEN)Quick start - launching server...$(NC)"
	@make run &
	@echo "$(BLUE)Waiting for server to start...$(NC)"
	@sleep 3
	@make test-upload
	@echo "$(GREEN)Quick start complete! Server is running.$(NC)"

setup: ## Initial project setup
	@echo "$(GREEN)Setting up project...$(NC)"
	@make install-deps
	@make install-target
	@make fmt
	@make check
	@echo "$(GREEN)Setup complete!$(NC)"

## Help
help: ## Show this help message
	@echo "$(GREEN)$(PROJECT_NAME) - Makefile Commands$(NC)"
	@echo ""
	@echo "$(GREEN)Build Commands:$(NC)"
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z_-]+:.*?## / && /Build/ {printf "  $(YELLOW)%-20s$(NC) %s\n", $\$1, $\$2}' $(MAKEFILE_LIST)
	@echo ""
	@echo "$(GREEN)Development Commands:$(NC)"
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z_-]+:.*?## / && /development|Development|test|Test|check|Check|format|Format|lint/ {printf "  $(YELLOW)%-20s$(NC) %s\n", $\$1, $\$2}' $(MAKEFILE_LIST)
	@echo ""
	@echo "$(GREEN)Docker Commands:$(NC)"
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z_-]+:.*?## / && /Docker|docker/ {printf "  $(YELLOW)%-20s$(NC) %s\n", $\$1, $\$2}' $(MAKEFILE_LIST)
	@echo ""
	@echo "$(GREEN)Utility Commands:$(NC)"
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z_-]+:.*?## / && !/Build|development|Development|test|Test|check|Check|format|Format|lint|Docker|docker/ {printf "  $(YELLOW)%-20s$(NC) %s\n", $\$1, $\$2}' $(MAKEFILE_LIST)
	@echo ""
	@echo "$(GREEN)Environment Variables:$(NC)"
	@echo "  $(YELLOW)LISTEN_ADDR$(NC)        Server listen address (default: 0.0.0.0:8080)"
	@echo "  $(YELLOW)UPLOAD_DIR$(NC)         Upload directory (default: ./uploads)"
	@echo "  $(YELLOW)MAX_FILE_SIZE$(NC)      Maximum file size in bytes (default: 256MB)"
	@echo "  $(YELLOW)FILE_EXPIRATION$(NC)    File expiration in seconds (default: 36 hours)"
	@echo "  $(YELLOW)MAX_DOWNLOAD_COUNT$(NC) Maximum downloads per file (default: 0=unlimited)"
	@echo "  $(YELLOW)API_KEY$(NC)            API key for authentication (optional)"
	@echo "  $(YELLOW)REDIS_ADDR$(NC)         Redis address (optional, e.g., localhost:6379)"
	@echo "  $(YELLOW)REDIS_DB$(NC)           Redis database number (default: 0)"
	@echo "  $(YELLOW)REGISTRY$(NC)           Docker registry for push (required for docker-push)"
	@echo ""
	@echo "$(GREEN)Examples:$(NC)"
	@echo "  make setup                    # Initial project setup"
	@echo "  make quick-start              # Build and test quickly"
	@echo "  make dev                      # Development with auto-reload"
	@echo "  make run API_KEY=secret123    # Run with API key"
	@echo "  make docker-run REDIS_ADDR=redis:6379"
	@echo "  make docker-push REGISTRY=docker.io/username"

status: ## Show project status
	@echo "$(GREEN)Project Status:$(NC)"
	@echo "Project: $(BLUE)$(PROJECT_NAME)$(NC)"
	@echo "Binary: $(BLUE)$(BINARY_NAME)$(NC)"
	@echo "Target: $(BLUE)$(RUST_TARGET)$(NC)"
	@echo ""
	@echo "$(GREEN)Build Status:$(NC)"
	@if [ -f "$(BUILD_DIR)/$(BINARY_NAME)" ]; then \
		echo "Production binary: $(GREEN)✓ Built$(NC)"; \
		ls -lh $(BUILD_DIR)/$(BINARY_NAME); \
	else \
		echo "Production binary: $(RED)✗ Not built$(NC)"; \
	fi
	@if [ -f "$(LOCAL_BUILD_DIR)/$(BINARY_NAME)" ]; then \
		echo "Local binary: $(GREEN)✓ Built$(NC)"; \
		ls -lh $(LOCAL_BUILD_DIR)/$(BINARY_NAME); \
	else \
		echo "Local binary: $(RED)✗ Not built$(NC)"; \
	fi
	@echo ""
	@echo "$(GREEN)Docker Status:$(NC)"
	@docker images | grep $(DOCKER_IMAGE) > /dev/null 2>&1 && \
		echo "Docker image: $(GREEN)✓ Built$(NC)" || \
		echo "Docker image: $(RED)✗ Not built$(NC)"
	@docker ps -a | grep $(PROJECT_NAME) > /dev/null 2>&1 && \
		echo "Docker container: $(GREEN)✓ Exists$(NC)" || \
		echo "Docker container: $(YELLOW)⚬ Not created$(NC)"
