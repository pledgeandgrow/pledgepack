# Contributing to PledgePack

Thank you for your interest in contributing to PledgePack! This document outlines the process for contributing to the project.

## Getting Started

### Prerequisites

- [Rust](https://rustup.rs/) (stable, edition 2024)
- [Zig](https://ziglang.org/) (0.16.0+)
- [Node.js](https://nodejs.org/) (>=18)

### Setup

```bash
# Clone the repository
git clone https://github.com/pledgeandgrow/pledgepack.git
cd pledgepack

# Build the Zig native library first (required by native-sys), then Rust
zig build -Doptimize=ReleaseFast
cargo build --release

# Run tests
cargo test

# Run benchmarks
zig build bench              # Zig-side micro-benchmarks
cargo bench -p pledgepack-core  # criterion benchmarks for the transform pipeline
```

> **Windows + Git Bash:** `rust-toolchain.toml` pins `channel = "stable"`,
> which resolves to the MSVC toolchain on Windows. In Git Bash, Git's
> coreutils `link.exe` (`/usr/bin/link`) shadows the MSVC linker in PATH and
> every build fails with `link: extra operand ...`. Either build with the
> GNU toolchain explicitly (`cargo +stable-x86_64-pc-windows-gnu build`),
> run cargo from PowerShell/cmd instead of Git Bash, or put MSVC's `link.exe`
> ahead of `Git\usr\bin` in PATH. Do **not** pin a host-specific toolchain
> string in `rust-toolchain.toml` — it applies on every OS and breaks
> non-Windows contributors and CI. `pledgepack doctor` detects this
> misconfiguration and prints the same guidance.

## Distribution & Publish Policy

PledgePack is **distributed only as an npm package** (`pledgepack`), which
downloads a prebuilt native `pledge` binary from the matching GitHub Release
at install time (`bin/postinstall.js`). The Rust crates in this workspace are
**not published to crates.io**, and `publish = false` in the workspace
`Cargo.toml` is intentional:

- it prevents an accidental `cargo publish` of internal crates;
- it lets `cargo deny`'s wildcard check pass without pinning a version on
  every intra-workspace path dependency;
- the crates are not a supported public Rust API — the CLI, the config file
  and the plugin interfaces (JS host, WASM/WIT contract) are.

Do not add a `CARGO_REGISTRY_TOKEN` or a `cargo publish` step to the release
workflow. If a crate ever needs to be published (for example the plugin SDK),
that is a deliberate, separately-reviewed decision: remove
`publish.workspace = true` from that crate only, give its internal
dependencies real `version` requirements, and document the new public API
contract first.

### Cutting a release

1. Bump the version everywhere at once: `Cargo.toml` (`[workspace.package]`),
   `package.json`, `platforms.json` (`version`), and refresh the lockfile
   (`cargo update -w`). `bash scripts/check-versions.sh [vX.Y.Z]` verifies all
   of them (and `Cargo.lock`) agree; CI runs it on every PR.
2. Add a `## [X.Y.Z] - YYYY-MM-DD` section to `docs/CHANGELOG.md`
   (`scripts/check-changelog-entry.sh` blocks the release without it).
3. **Rehearse first:** Actions → *Release* → *Run workflow* with the version and
   `dry_run: true` (the default). This runs the security gate, builds all six
   platforms, verifies the archive set and runs `npm publish --dry-run` —
   without signing, tagging, creating a GitHub release or publishing.
4. Push the tag `vX.Y.Z` to run the real release (signs artifacts with cosign,
   creates the GitHub Release, publishes to npm with provenance). Prerelease
   versions (e.g. `0.5.0-rc.1`) are published under the matching npm dist-tag
   (`rc`), never `latest`.

## Development Workflow

### 1. Create a Branch

```bash
git checkout -b feat/your-feature-name
```

Use the following prefixes:
- `feat/` — New features
- `fix/` — Bug fixes
- `docs/` — Documentation changes
- `refactor/` — Code refactoring
- `test/` — Test additions or fixes
- `chore/` — Build, CI, or tooling changes

### 2. Make Your Changes

- Follow the existing code style and patterns
- Add tests for new functionality
- Update documentation as needed
- Ensure `cargo test` passes
- Ensure `cargo clippy` passes without warnings

### 3. Commit Your Changes

We use [Conventional Commits](https://www.conventionalcommits.org/):

```
feat: add support for Preact adapter
fix: resolve import path edge case with trailing slash
docs: update ARCHITECTURE.md with polyfills module
refactor: simplify transform pipeline error handling
test: add unit tests for CSS module hash generation
chore: update oxc dependency to latest version
```

### 4. Push and Create a Pull Request

```bash
git push origin feat/your-feature-name
```

Then create a pull request on GitHub with:
- A clear title following conventional commit format
- A description of what changed and why
- Any breaking changes noted
- Links to related issues

## Code Style

### Rust

- Follow `rustfmt` defaults (run `cargo fmt`)
- Follow `clippy` recommendations (run `cargo clippy`)
- Use `anyhow::Result` for error handling in application code
- Use `thiserror` for library error types
- Prefer `tracing` over `log` for structured logging
- Document public APIs with `///` doc comments

### Zig

- Follow the [Zig Style Guide](https://ziglang.org/documentation/master/#Style-Guide)
- Use snake_case for functions and variables
- Use TitleCase for types
- Document public functions with `///` comments

### TypeScript/JavaScript

- Use TypeScript for all new code
- Follow the existing ESLint configuration
- Use ESM (`import`/`export`) syntax

## Project Structure

```
crates/
├── cli/                 # CLI entry point (pledgepack-cli; binary: pledge)
├── core/                # Core engine, config, transform, pipeline
├── cache/               # Function-level incremental cache
├── resolver/            # Module resolution
├── dev-server/          # Dev server + HMR
├── optimizer/           # Tree shaking, code splitting
├── js-plugin-host/      # JS plugin system (QuickJS via rquickjs)
├── wasm-plugin-host/    # WASM plugin host (wasmtime, Component Model)
├── task-system/         # Parallel task execution engine
├── task-system-macros/  # #[task] proc macros for task-system
├── adapter-react/       # React adapter
├── adapter-solid/       # Solid.js adapter
├── adapter-next/        # Next.js adapter
├── adapter-tanstack/    # TanStack Router adapter
└── adapter-pledgestack/ # PledgeStack adapter (route discovery + manifest)
native-sys/              # Zig FFI bindings
docs/                    # Documentation
```

## Testing

```bash
# Run all Rust tests
cargo test

# Run tests for a specific crate
cargo test -p pledgepack-core

# Run tests with output
cargo test -- --nocapture

# Run benchmarks
cargo bench
```

## Reporting Issues

When reporting issues, please include:
- PledgePack version (`pledgepack --version`, or `target/release/pledge --version` for a locally-built binary)
- Operating system
- Rust version (`rustc --version`)
- Zig version (`zig version`)
- Minimal reproduction case
- Expected vs actual behavior

## License

By contributing, you agree that your contributions will be licensed under the MIT License.

## Questions?

Feel free to open a discussion on GitHub or reach out to the maintainers.
