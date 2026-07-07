NIGHTLY := +$(shell cat nightly-toolchain)

# ── Formatting ───────────────────────────────────────────────────────────────

fmt:
	cargo $(NIGHTLY) fmt --all
	taplo fmt
	cargo sort --workspace --no-format

fmt-check:
	cargo $(NIGHTLY) fmt --all -- --check
	taplo fmt --check
	cargo sort --workspace --no-format --check

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

clean-artifacts:
	rm -rf sample_proj/out sample_proj/cache

.PHONY: fmt fmt-check clippy check build test flush-cache