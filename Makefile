APP=tmpfile
IMAGE=proxyxai/tmpfile
VERSION?=rust
SANITIZED_VERSION:=$(subst :,-,$(VERSION))
TOOLCHAIN?=stable
CARGO:=cargo +$(TOOLCHAIN)
CLIPPY_ENV:=
FEATURE_ARGS:=--all-features

.PHONY: build run clean fmt fmt-check check test \
        clippy clippy-ci lint audit release-check \
        docker-build docker-test docker-run

# ============================================================
#  Clippy Lint Groups
# ============================================================

# 发布门禁：只保留核心风险项，避免纯风格项阻断发版
CLIPPY_CORE_DENY = \
	-D warnings                            \
	-D clippy::correctness                 \
	-D clippy::perf                        \
	-D clippy::style

CLIPPY_WARN = \
	-W clippy::pedantic                    \
	-W clippy::nursery

CLIPPY_PERF = \
	-D clippy::inefficient_to_string       \
	-D clippy::implicit_clone              \
	-D clippy::cloned_instead_of_copied    \
	-D clippy::redundant_clone             \
	-D clippy::large_enum_variant          \
	-D clippy::large_types_passed_by_value \
	-D clippy::needless_pass_by_value      \
	-D clippy::unnecessary_wraps           \
	-D clippy::rc_buffer                   \
	-D clippy::rc_mutex                    \
	-D clippy::mutex_atomic                \
	-D clippy::linkedlist                  \
	-D clippy::trivially_copy_pass_by_ref  \
	-D clippy::box_collection

CLIPPY_SAFETY = \
	-D clippy::undocumented_unsafe_blocks     \
	-D clippy::multiple_unsafe_ops_per_block  \
	-D clippy::dbg_macro                      \
	-W clippy::as_conversions                 \
	-W clippy::cast_possible_truncation       \
	-W clippy::cast_precision_loss            \
	-W clippy::cast_sign_loss

CLIPPY_CLEAN = \
	-D clippy::manual_let_else                \
	-D clippy::semicolon_if_nothing_returned  \
	-D clippy::flat_map_option                \
	-D clippy::map_unwrap_or                  \
	-D clippy::needless_continue              \
	-D clippy::redundant_else                 \
	-D clippy::match_same_arms

CLIPPY_ALLOW = \
	-A clippy::module_name_repetitions  \
	-A clippy::must_use_candidate       \
	-A clippy::missing_errors_doc       \
	-A clippy::missing_panics_doc       \
	-A clippy::option_if_let_else       \
	-A clippy::redundant_pub_crate      \
	-A clippy::doc_markdown

# 开发态：保留更广覆盖（pedantic/nursery 仅告警，不作为门禁）
CLIPPY_DEV_DENY = \
	-D clippy::correctness               \
	-D clippy::perf                      \
	-D clippy::style

CLIPPY_DEV_FLAGS = \
	$(CLIPPY_DEV_DENY) $(CLIPPY_WARN) \
	$(CLIPPY_PERF) $(CLIPPY_SAFETY) $(CLIPPY_CLEAN) $(CLIPPY_ALLOW)

CLIPPY_CI_FLAGS = \
	$(CLIPPY_CORE_DENY) $(CLIPPY_ALLOW)

# ============================================================
#  Dev
# ============================================================

build:
	$(CARGO) build --locked --release

run:
	$(CARGO) run --locked

clean:
	$(CARGO) clean

check:
	$(CARGO) check --locked --all-targets $(FEATURE_ARGS)

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all -- --check

test:
	$(CARGO) test --locked --all-targets $(FEATURE_ARGS)

# ============================================================
#  Lint
# ============================================================

clippy:
	$(CLIPPY_ENV) $(CARGO) clippy --locked --all-targets $(FEATURE_ARGS) \
		--message-format short -- $(CLIPPY_DEV_FLAGS)

clippy-ci:
	$(CLIPPY_ENV) $(CARGO) clippy --locked --all-targets $(FEATURE_ARGS) \
		--message-format short -- $(CLIPPY_CI_FLAGS)

lint: fmt-check clippy

audit:
	$(CARGO) audit

# ============================================================
#  Release Gate
# ============================================================

release-check: fmt-check test clippy-ci audit
	@echo ""
	@echo "=========================================="
	@echo "  OK  All checks passed. Ready to ship."
	@echo "=========================================="

# ============================================================
#  Docker
# ============================================================

docker-build:
	docker build --pull -t $(IMAGE):$(SANITIZED_VERSION) .
	docker push $(IMAGE):$(SANITIZED_VERSION)

docker-test:
	docker build --pull -t $(IMAGE):$(SANITIZED_VERSION)-test .
	docker push $(IMAGE):$(SANITIZED_VERSION)-test

docker-run:
	docker run --rm -p 8080:8080 $(IMAGE):$(SANITIZED_VERSION)
