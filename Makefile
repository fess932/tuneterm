# tuneterm — run `make` for the list of targets.
#
# Cargo already does the real work; these are shortcuts for the things worth
# remembering, and `make check` is exactly what CI runs.

BIN     := tuneterm
MUSIC   ?=
CACHE   := $(HOME)/Library/Caches/$(BIN)

.DEFAULT_GOAL := help
.PHONY: help run dev install uninstall build check test fmt lint bench scan clean clean-cache release-dry

help: ## Show this help
	@printf '\033[1m%s\033[0m\n' 'tuneterm'
	@grep -hE '^[a-z-]+:.*?## ' $(MAKEFILE_LIST) \
		| awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2}'
	@printf '\n  Pass a folder with MUSIC=path, e.g. make run MUSIC=~/Downloads\n'

run: ## Build in release mode and run (release: covers draw ~30x faster)
	cargo run --release -- $(MUSIC)

dev: ## Run a debug build, for iterating on the code
	cargo run -- $(MUSIC)

install: ## Install into ~/.cargo/bin, which is already on PATH
	cargo install --path . --locked
	@printf '\n%s\n' "installed: $$(command -v $(BIN) || echo '~/.cargo/bin/$(BIN)')"
	@$(BIN) --version

uninstall: ## Remove the installed binary
	cargo uninstall $(BIN)

build: ## Release build, without installing
	cargo build --release
	@ls -lh target/release/$(BIN)

check: fmt lint test ## Everything CI checks: fmt, clippy, tests

test: ## Run the test suite
	cargo test

fmt: ## Check formatting (does not rewrite; use `cargo fmt` for that)
	cargo fmt --all -- --check

lint: ## Clippy, warnings as errors
	cargo clippy --all-targets -- -D warnings

bench: ## Run the ignored benchmarks and print their numbers
	cargo test --release -- --ignored --nocapture

scan: ## Headless dump of folders, tags and cover sizes
	cargo run --release -- $(MUSIC) --scan

clean: ## Remove build output
	cargo clean

clean-cache: ## Empty the cover cache
	rm -rf "$(CACHE)"
	@echo "removed $(CACHE)"

release-dry: ## Build and package a tarball the way the release workflow does
	@set -eu; \
	target=$$(rustc -vV | awk '/^host:/ {print $$2}'); \
	cargo build --release --target "$$target"; \
	staging="$(BIN)-$$target"; \
	rm -rf "dist/$$staging" "dist/$$staging.tar.gz"; \
	mkdir -p "dist/$$staging"; \
	cp "target/$$target/release/$(BIN)" README.md LICENSE "dist/$$staging/"; \
	tar czf "dist/$$staging.tar.gz" -C dist "$$staging"; \
	shasum -a 256 "dist/$$staging.tar.gz"; \
	ls -lh "dist/$$staging.tar.gz"
