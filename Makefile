.PHONY: help build test smoke ci clean

help:
	@echo "Targets:"
	@echo "  make build  cargo build --release"
	@echo "  make test   cargo test --release (73 unit + 3 integration)"
	@echo "  make smoke  clean-room artifact-as-shipped gate (mktemp + run + assert)"
	@echo "  make ci     build + test + smoke"
	@echo "  make clean  cargo clean"

build:
	cargo build --release

test:
	cargo test --release

smoke: build
	bash scripts/smoke.sh

ci: build test smoke

clean:
	cargo clean
