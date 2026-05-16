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

- Run `cargo fmt` before committing
- Run `cargo clippy` and address warnings
- Follow existing patterns in the codebase

## License

By contributing, you agree that your contributions will be licensed under the Apache License 2.0, the same license as the project.
