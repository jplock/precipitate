# Developer entry points. Every target runs the same commands CI and the
# prek hooks run, so `make check` passing locally means the commit passes.

CARGO ?= cargo
TARGETS_MUSL := x86_64-unknown-linux-musl aarch64-unknown-linux-musl

.PHONY: all build check fmt fmt-check clippy test deny run release release-musl clean hooks

all: check

build:
	$(CARGO) build --workspace

## Quality gates

check: fmt-check clippy test deny

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all -- --check

clippy:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

test:
	$(CARGO) test --workspace

deny:
	$(CARGO) deny check

## Running

# make run ARGS="--profile mgmt"        opens the TUI
# make run ARGS="scan --profile mgmt"   headless scan
run:
	$(CARGO) run -p precip -- $(ARGS)

## Release builds

# Host-native release binary (macOS, Windows, or dynamically linked Linux).
release:
	$(CARGO) build --release -p precip

# Static Linux binaries. Requires cargo-zigbuild and zig; the bundled SQLite
# and mimalloc C sources are compiled by zig cc against musl.
release-musl:
	@for t in $(TARGETS_MUSL); do \
		echo "==> $$t"; \
		$(CARGO) zigbuild --release -p precip --target $$t || exit 1; \
	done
	@ls -la $(addprefix target/,$(addsuffix /release/precip,$(TARGETS_MUSL)))

## Housekeeping

hooks:
	prek install

clean:
	$(CARGO) clean
