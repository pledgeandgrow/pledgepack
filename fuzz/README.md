# PledgePack fuzz targets

Fuzz targets for code that parses input PledgePack doesn't control — import
specifiers and `package.json` content from whatever the user (or a
transitive dependency) has installed. See
[`../docs/PRODUCTION-READINESS-100.md`](../docs/PRODUCTION-READINESS-100.md)
goal 29.

## Setup (one-time)

```bash
rustup install nightly
cargo install cargo-fuzz
```

## Running

From the `fuzz/` directory:

```bash
cargo +nightly fuzz run resolve_specifier
cargo +nightly fuzz run package_json_exports
cargo +nightly fuzz run js_config_eval
cargo +nightly fuzz run export_check
cargo +nightly fuzz run migrate_config
```

Or run whichever targets are registered:

```bash
for t in $(cargo +nightly fuzz list); do cargo +nightly fuzz run "$t"; done
```

Each runs until stopped (Ctrl+C) or a crash is found. A found crash is
written under `artifacts/<target-name>/` with a reproducer input — re-run
against just that input with:

```bash
cargo +nightly fuzz run resolve_specifier artifacts/resolve_specifier/<crash-file>
```

## Targets

- **`resolve_specifier`** — fuzzes the specifier string passed to
  `Resolver::resolve()` against a small fixed fixture project (plain files,
  a scoped package with a conditional `exports` map, a path alias, and — on
  Unix — a deliberately broken symlink).
- **`package_json_exports`** — fuzzes the *content* of a `package.json`
  file's `exports` field (and the rest of the file — the fuzzer isn't
  constrained to produce valid JSON, since the resolver has to handle that
  too), exercising the conditional-exports parsing path specifically.
- **`js_config_eval`** — feeds arbitrary bytes as a `pledge.config` module
  body to `js_config::eval_config_module`; asserts the static evaluator
  never panics on malformed, truncated, or adversarial JS.
- **`export_check`** — pairs a fuzzed source module with a fuzzed import
  statement and runs `export_check` diagnostics; asserts no panic and that
  termination is guaranteed on any byte sequence.
- **`migrate_config`** — feeds fuzzed Vite/webpack-style config source to
  the migration static evaluator; same no-panic/must-terminate contract.

## CI

`ci.yml`'s `fuzz-smoke` job is **enforced** (no `continue-on-error`): it builds
every target with `cargo +nightly fuzz build` and then runs each for 30 seconds
(`-max_total_time=30`), so a target that stops compiling — or a shallow crash —
fails CI. Long fuzzing sessions (hours) are a manual activity; run them locally
as described above and commit any reproducer as a regression test.
