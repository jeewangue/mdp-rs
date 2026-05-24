.PHONY: help build test clippy smoke ci clean

help:
	@echo "Targets:"
	@echo "  make build   cargo build --release"
	@echo "  make test    cargo test --release"
	@echo "  make clippy  cargo clippy --all-targets -- -D warnings"
	@echo "  make smoke   clean-room artifact-as-shipped gate (mktemp + run + assert)"
	@echo "  make ci      clippy + build + test + smoke"
	@echo "  make clean   cargo clean"

build:
	cargo build --release

test:
	cargo test --release

clippy:
	cargo clippy --all-targets -- -D warnings

smoke: build
	bash scripts/smoke.sh

ci: clippy build test smoke

clean:
	cargo clean
