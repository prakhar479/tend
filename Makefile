.PHONY: check check-clean fmt lint test build release e2e

check: ## fmt + lint + test. Run before you call a task done.
	cargo fmt --all --check
	cargo clippy --all-targets -- -D warnings
	cargo test

check-clean: ## make check as CI sees it: empty HOME, minimal PATH (no local pytest, claude, …)
	@home=$$(mktemp -d); env -i HOME=$$home PATH=$$(dirname $$(command -v cargo)):/usr/bin:/bin \
		RUSTUP_HOME=$${RUSTUP_HOME:-$$HOME/.rustup} CARGO_HOME=$${CARGO_HOME:-$$HOME/.cargo} \
		$(MAKE) check; status=$$?; rm -rf $$home; exit $$status

fmt:
	cargo fmt --all

lint:
	cargo clippy --all-targets -- -D warnings

test:
	cargo test

e2e:
	cargo test -p tend-cli --test e2e

build:
	cargo build

release:
	cargo build --release
