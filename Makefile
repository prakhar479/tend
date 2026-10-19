.PHONY: check fmt lint test build release e2e

check: ## fmt + lint + test. Run before you call a task done.
	cargo fmt --all --check
	cargo clippy --all-targets -- -D warnings
	cargo test

fmt:
	cargo fmt --all

lint:
	cargo clippy --all-targets -- -D warnings

test:
	cargo test

e2e:
	cargo test -p winnow --test e2e

build:
	cargo build

release:
	cargo build --release
