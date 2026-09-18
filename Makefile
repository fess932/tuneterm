# tuneterm — run `make` for the list of targets.
#
# Cargo already does the real work; these are shortcuts for the things worth
# remembering, and `make check` is exactly what CI runs.
#
# Everything works on macOS, Linux and Windows. The cargo targets are identical
# everywhere, so only the handful of one-liners that touch the filesystem or
# print something are defined per platform, in the block below; the rules
# themselves stay single-copy. On Windows the shell is pinned to cmd.exe —
# otherwise make picks sh.exe when Git's tools happen to be on PATH and cmd.exe
# when they are not — and the Unix commands give way to PowerShell.

BIN     := tuneterm
MUSIC   ?=
# podman works the same: make image DOCKER=podman
DOCKER  ?= docker
IMAGE   ?= tuneterm-server

ifeq ($(OS),Windows_NT)

SHELL       := cmd.exe
.SHELLFLAGS := /C
EXE         := .exe
CACHE       := $(LOCALAPPDATA)\$(BIN)
PS          := powershell -NoProfile -ExecutionPolicy Bypass -Command

TITLE     := echo tuneterm
BLANK     := echo.
HINT      := echo   Pass a folder with MUSIC=path, e.g. make run MUSIC=%USERPROFILE%\Music
define LIST_TARGETS
$(PS) "Get-Content '$(firstword $(MAKEFILE_LIST))' | Select-String -Pattern '^([a-z-]+):.*## (.*)' | ForEach-Object { '  {0,-14} {1}' -f $$_.Matches.Groups[1].Value, $$_.Matches.Groups[2].Value }"
endef

SHOW_INSTALLED := $(PS) "$$p = (Get-Command $(BIN) -ErrorAction SilentlyContinue).Source; if (-not $$p) { $$p = Join-Path $$env:USERPROFILE '.cargo\bin\$(BIN)$(EXE)' }; 'installed: ' + $$p"
SHOW_BIN       := $(PS) "Get-Item 'target\release\$(BIN)$(EXE)' | ForEach-Object { '{0}  {1:N1} MB' -f $$_.FullName, ($$_.Length / 1MB) }"
RM_CACHE       := $(PS) "if (Test-Path '$(CACHE)') { Remove-Item -Recurse -Force '$(CACHE)' }"

define PACKAGE
$(PS) "$$ErrorActionPreference = 'Stop'; \
	$$target = ((rustc -vV | Select-String '^host: ').Line -split ' ')[1]; \
	cargo build --release --target $$target; if ($$LASTEXITCODE) { exit $$LASTEXITCODE }; \
	$$staging = 'dist/$(BIN)-' + $$target; \
	Remove-Item -Recurse -Force $$staging, ($$staging + '.tar.gz') -ErrorAction SilentlyContinue; \
	New-Item -ItemType Directory -Force $$staging | Out-Null; \
	Copy-Item ('target/' + $$target + '/release/$(BIN)$(EXE)'), 'README.md', 'LICENSE' $$staging; \
	tar czf ($$staging + '.tar.gz') -C dist ('$(BIN)-' + $$target); \
	$$bytes = [IO.File]::ReadAllBytes((Resolve-Path ($$staging + '.tar.gz')).Path); \
	[BitConverter]::ToString([Security.Cryptography.SHA256]::Create().ComputeHash($$bytes)).Replace('-', '').ToLower() + '  ' + $$staging + '.tar.gz'; \
	Get-Item ($$staging + '.tar.gz') | ForEach-Object { '{0}  {1:N1} MB' -f $$_.Name, ($$_.Length / 1MB) }"
endef

else

EXE :=
ifeq ($(shell uname -s),Darwin)
CACHE := $(HOME)/Library/Caches/$(BIN)
else
CACHE := $(if $(XDG_CACHE_HOME),$(XDG_CACHE_HOME),$(HOME)/.cache)/$(BIN)
endif

TITLE     := printf '\033[1m%s\033[0m\n' 'tuneterm'
BLANK     := printf '\n'
HINT      := printf '  Pass a folder with MUSIC=path, e.g. make run MUSIC=~/Downloads\n'
define LIST_TARGETS
grep -hE '^[a-z-]+:.*?## ' $(MAKEFILE_LIST) \
	| awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2}'
endef

SHOW_INSTALLED := printf '%s\n' "installed: $$(command -v $(BIN) || echo '~/.cargo/bin/$(BIN)')"
SHOW_BIN       := ls -lh target/release/$(BIN)
RM_CACHE       := rm -rf "$(CACHE)"

define PACKAGE
set -eu; \
	target=$$(rustc -vV | awk '/^host:/ {print $$2}'); \
	cargo build --release --target "$$target"; \
	staging="dist/$(BIN)-$$target"; \
	rm -rf "$$staging" "$$staging.tar.gz"; \
	mkdir -p "$$staging"; \
	cp "target/$$target/release/$(BIN)$(EXE)" README.md LICENSE "$$staging/"; \
	tar czf "$$staging.tar.gz" -C dist "$(BIN)-$$target"; \
	if command -v shasum >/dev/null 2>&1; then shasum -a 256 "$$staging.tar.gz"; \
	else sha256sum "$$staging.tar.gz"; fi; \
	ls -lh "$$staging.tar.gz"
endef

endif

.DEFAULT_GOAL := help
.PHONY: help run dev install uninstall build check test fmt lint bench scan clean clean-cache release-dry serve image image-run

help: ## Show this help
	@$(TITLE)
	@$(LIST_TARGETS)
	@$(BLANK)
	@$(HINT)

run: ## Build in release mode and run (release: covers draw ~30x faster)
	cargo run --release -- $(MUSIC)

dev: ## Run a debug build, for iterating on the code
	cargo run -- $(MUSIC)

install: ## Install into ~/.cargo/bin, which is already on PATH
	cargo install --path . --locked
	@$(BLANK)
	@$(SHOW_INSTALLED)
	@$(BIN) --version

uninstall: ## Remove the installed binary
	cargo uninstall $(BIN)

build: ## Release build, without installing
	cargo build --release
	@$(SHOW_BIN)

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

serve: ## Serve MUSIC over gRPC on :7700, without Docker
	cargo run --release -- serve $(MUSIC)

image: ## Build the server's Docker image (DOCKER=podman works too)
	$(DOCKER) build -t $(IMAGE) .

image-run: ## Run the image, serving MUSIC on :7700
	$(DOCKER) run --rm -p 7700:7700 -v "$(MUSIC):/music" -e TUNETERM_TOKEN $(IMAGE)

clean: ## Remove build output
	cargo clean

clean-cache: ## Empty the cover cache
	@$(RM_CACHE)
	@echo removed $(CACHE)

# Mirrors the packaging step in .github/workflows/release.yml: same layout, same
# tarball name, same sha256 — so a break here is a break there.
release-dry: ## Build and package a tarball the way the release workflow does
	@$(PACKAGE)
