# Contributing to xtafkit

Thank you for your interest in contributing! This document explains how the project is organized and how to submit changes.

## Branching Model

| Branch | Purpose | Who sees it |
|--------|---------|-------------|
| `main` | Default branch — current development | Everyone |

The project is small enough to track on a single branch; feature work happens on short-lived branches off `main`.

## How to Contribute

1. Fork the repository
2. Create a feature branch from `main`: `git checkout -b my-feature main`
3. Make your changes
4. Run the test suite: `cargo test --workspace`
5. Submit a pull request targeting `main`

## Development Setup

```bash
git clone https://github.com/rdmrocha/xtafkit.git
cd xtafkit
bash setup.sh
```

## Testing

All changes must pass the full test suite:

```bash
cargo test --workspace
```

For manual testing with a physical Xbox 360 drive:

```bash
cargo build --release
sudo ./target/release/xtafkit scan /dev/rdiskN
sudo ./target/release/xtafkit ls /dev/rdiskN --partition "360 Data" /
```

## Code Style

Every commit must pass formatting and clippy with no warnings:

```bash
cargo fmt --all -- --check                              # must produce no diff
cargo clippy --workspace --all-targets -- -D warnings   # must exit clean
```

Install the project's pre-commit hook to enforce this locally (it also runs the test suite):

```bash
git config core.hooksPath .githooks
```

CI runs the same checks; PRs that don't pass will be sent back. Beyond that:

- Follow existing patterns in the codebase.
- Don't `#[allow(clippy::...)]` to silence a lint without first understanding what it's catching — real fixes only, except for genuinely-misfiring lints (rare).

## License

By contributing, you agree that your contributions will be licensed under the Apache License 2.0, the same license as the project.
