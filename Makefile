_nightly := $(shell grep -Ex 'nightly-[0-9]{4}-[0-9]{2}-[0-9]{2}' nightly-toolchain)
$(if $(_nightly),,$(error nightly-toolchain: content does not match nightly-YYYY-MM-DD))
NIGHTLY := +$(_nightly)
TAPLO_VERSION := 0.10.0
CARGO_SORT_VERSION := 2.1.4
CARGO_DENY_VERSION := 0.20.2
OSV_SCANNER_VERSION := 2.3.2

# ── Formatting ───────────────────────────────────────────────────────────────

install-fmt-tools:
	cargo install taplo-cli --version $(TAPLO_VERSION) --locked
	cargo install cargo-sort --version $(CARGO_SORT_VERSION) --locked

install-deny:
	cargo install cargo-deny --version $(CARGO_DENY_VERSION) --locked

fmt:
	cargo $(NIGHTLY) fmt --all
	taplo fmt
	cargo sort --workspace --no-format

fmt-check:
	cargo $(NIGHTLY) fmt --all -- --check
	taplo fmt --check
	cargo sort --workspace --no-format --check

# ── Dependency auditing ───────────────────────────────────────────────────────

deny:
	cargo deny check

osv:
	osv-scanner scan --config osv-scanner.toml --lockfile Cargo.lock

# ── Linting ──────────────────────────────────────────────────────────────────

clippy:
	cargo clippy --all-targets --all-features -- -D warnings

# ── Build ─────────────────────────────────────────────────────────────────────

check:
	cargo check --all-targets --all-features

build:
	cargo build

# ── Tests ─────────────────────────────────────────────────────────────────────

test:
	cargo run --example macros -- test --root sample_proj

coverage:
	cargo run --example macros -- coverage --root sample_proj

snapshot:
	cargo run --example macros -- snapshot --root sample_proj --snap target/.gas-snapshot-smoke
	cargo run --example macros -- snapshot --root sample_proj --check target/.gas-snapshot-smoke

clean-artifacts:
	rm -rf sample_proj/out sample_proj/cache

.PHONY: fmt fmt-check clippy check build test clean-artifacts snapshot coverage install-fmt-tools install-deny deny osv