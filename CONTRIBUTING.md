# Contributing to reforge

## Getting started

```sh
git clone https://github.com/ava-labs/reforge
cd reforge
git submodule update --init --recursive  # initialises forge-std under sample_proj/
cargo build
```

## Running checks locally

```sh
make fmt          # format
make fmt-check    # check formatting (CI gate)
make clippy       # lint
make test         # run example test suite
make deny         # dependency audit
```

## Pull requests

- Open a PR against `main`
- All CI jobs must pass (Example, Clippy, Formatting, Dependency audit)
- At least one approving review is required
- Commits to `main` are not allowed directly

## Reporting bugs

Use the [bug report template](.github/ISSUE_TEMPLATE/bug_report.md).

## Security vulnerabilities

Do **not** open a public issue. See [SECURITY.md](SECURITY.md).

## License

By contributing you agree that your contributions will be licensed under the
MIT License. See [LICENSE](LICENSE).