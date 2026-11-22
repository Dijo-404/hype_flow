.PHONY: test check install release check-python check-desktop

CARGO ?= $(or $(shell command -v cargo 2>/dev/null),$(HOME)/.cargo/bin/cargo)
export PKG_CONFIG_PATH := $(HOME)/.local/lib/pkgconfig$(if $(PKG_CONFIG_PATH),:$(PKG_CONFIG_PATH))
export CARGO_BUILD_JOBS ?= 2

test:
	$(CARGO) test --workspace

check: test
	$(CARGO) fmt --check
	$(CARGO) clippy --workspace --all-targets -- -D warnings

check-python:
	PYTHONPATH=src:. python3 -m unittest discover -s tests -v
	python3 -m compileall -q src/dictation tools

release:
	$(CARGO) build --release --workspace

check-desktop:
	sh tools/check-desktop.sh

install: release
	sh tools/install.sh
