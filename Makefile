.PHONY: build test fmt lint dist wheel dist-all build-binaries build-wheels publish clean help

.DEFAULT_GOAL := help

COMMIT_ID=$(shell git rev-parse --short=8 HEAD 2>/dev/null || echo dev)
# Linux binaries are static musl builds, so one per architecture runs on any distro. Apple
# targets link against the macOS SDK and build only on a Mac; override the list elsewhere.
DIST_TARGETS ?= x86_64-unknown-linux-musl aarch64-unknown-linux-musl \
	x86_64-apple-darwin aarch64-apple-darwin
# gnu -> manylinux2014 wheels, musl -> musllinux_1_2 wheels (Alpine), apple -> macOS wheels.
WHEEL_TARGETS ?= x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu \
	x86_64-unknown-linux-musl aarch64-unknown-linux-musl \
	x86_64-apple-darwin aarch64-apple-darwin
MATURIN_VERSION = 1.15.0
ZIGLANG_VERSION = 0.16.0
CARGO_ZIGBUILD_VERSION = 0.23.4
TWINE_VERSION = 6.1.0

# Linux targets cross-compile with zig from Linux and macOS hosts alike.
ZIGBUILD = uvx --with ziglang==$(ZIGLANG_VERSION) cargo-zigbuild@$(CARGO_ZIGBUILD_VERSION) zigbuild
MATURIN = uvx --with ziglang==$(ZIGLANG_VERSION) maturin@$(MATURIN_VERSION) build --release --out dist

build: ## debug build
	cargo build

test: ## run tests
	cargo test

fmt: ## apply clippy fixes and format
	cargo clippy --fix --allow-dirty
	cargo fmt

lint: ## check formatting and lints, fail on warnings (for use in e.g. ci/cd)
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings

dist: clean lint test build-binaries ## lint, test and build optimized binaries into ./dist (override with DIST_TARGETS)

wheel: clean lint test build-wheels ## lint, test and build the wheels into ./dist (override with WHEEL_TARGETS)

dist-all: clean lint test build-binaries build-wheels ## lint, test and build binaries and wheels into ./dist

# The build steps without clean/lint/test, so dist-all cleans only once.
build-binaries:
	mkdir -p dist
	set -e; for t in $(DIST_TARGETS); do \
		case $$t in \
			*-apple-darwin) cargo build --release --target $$t ;; \
			*) $(ZIGBUILD) --release --target $$t ;; \
		esac; \
		cp target/$$t/release/gcs-pypi dist/gcs-pypi-$(COMMIT_ID)-$$t; \
	done

build-wheels:
	set -e; for t in $(WHEEL_TARGETS); do \
		case $$t in \
			*-apple-darwin) $(MATURIN) --target $$t ;; \
			*-linux-gnu) $(MATURIN) --zig --target $$t --compatibility manylinux2014 ;; \
			*-linux-musl) $(MATURIN) --zig --target $$t --compatibility musllinux_1_2 ;; \
			*) echo "unsupported wheel target: $$t" >&2; exit 1 ;; \
		esac; \
	done

# Uploads what `make wheel` left in ./dist without rebuilding, so a failed upload can be retried.
# Only the wheels: the raw binaries in ./dist are not Python distributions.
publish: ## check and upload the wheels in ./dist to PyPI (run release.sh instead)
	uvx twine@$(TWINE_VERSION) check --strict dist/*.whl
	uvx twine@$(TWINE_VERSION) upload dist/*.whl

clean: ## clean and remove dist dir
	cargo clean
	rm -rf dist

help: ## this help
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z0-9._-]+:.*?## / {printf "\033[1m\033[36m%-38s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)
