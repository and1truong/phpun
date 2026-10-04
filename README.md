# phpun

**Bun for PHP — a modern PHP runtime and toolchain written in Rust.**

Status: **experimental — not production-ready.**

Primary engineering goal: **pass the PHP 8.5 official PHPT compatibility
suite.** The test suite is the executable specification; compatibility is
tracked continuously as `PHPT passed / applicable PHPT tests`.

## Current compatibility

`tests/lang` (core language): **274 / 280 applicable tests — 97.9%**
(reference PHP 8.5.11). Regenerate with the harness command below;
see `docs/compatibility.md` for what works, what fails, and the next
highest-leverage semantic cluster.

## Quick start

```sh
cargo build --release

# run a PHP script
./target/release/phpun hello.php

# vendor the php-src PHPT suite (once; ~120MB, gitignored)
./scripts/fetch-php-tests.sh php-8.5.11

# run the PHPT harness against phpun, with reference PHP for diffs
./target/release/phpun phpt vendor/php-tests/tests/lang \
    --sut ./target/release/phpun --php /path/to/php -j 8

# format PHP source PSR-12-style (stdout, or check/diff/write)
./target/release/phpun fmt src/ --diff
./target/release/phpun fmt -w src/
```

`phpun fmt` is a token-stream formatter built on phpun's own lexer
(no tree-sitter): 4-space indent, braces, spacing, PSR-12-ish. It is
idempotent and never invents or drops tokens — only whitespace and
line breaks change. `--check` prints unformatted files and exits 1
(for CI), `--diff` prints a unified diff, `-w` writes in place. With
no flag the formatted source goes to stdout. Directory arguments are
walked for `*.php` (skipping `vendor/` and dot-dirs).

A reproducible PHP 8.5 reference binary is needed for SKIPIF evaluation
and differential diffs (`--php`). Any `php` 8.5 on PATH works (e.g. via
Homebrew `php@8.5` or a source build).

## Architecture

Tree-walking interpreter (correctness first; a bytecode VM is a later
milestone):

```
PHP source → lexer → parser → AST → interpreter (Rust, no Zend Engine)
```

| Crate          | Contents                                            |
|----------------|-----------------------------------------------------|
| `phpun-core`   | lexer, parser, AST, values, interpreter, builtins   |
| `phpun-phpt`   | PHPT parser, EXPECT/EXPECTF/EXPECTREGEX matcher,    |
|                | runner (INI/ARGS/ENV/CLEAN/SKIPIF), diff, report    |
| `phpun`        | CLI: `phpun <file.php>`, `phpun phpt <paths>`,      |
|                | `phpun fmt <paths>`                                 |

The PHPT runner supports `FILE`, `EXPECT`, `EXPECTF`, `EXPECTREGEX`,
`SKIPIF`, `INI`, `ARGS`, `ENV`, `CLEAN`, captures stdout/stderr/exit
code/timeout, classifies unsupported sections explicitly, emits a
machine-readable JSON report plus compact diffs for failures.

## Roadmap

- [x] M0: repository, Rust workspace, CLI, CI
- [x] M1: PHPT runner + reference differential harness
- [x] M2: basic PHP programs execute
- [ ] M3: meaningful subset of `tests/lang` passes — **in progress (50%)**
- [ ] M4: core Zend language semantics
- [ ] M5: core runtime + `ext/standard` subset
- [ ] M6: Composer executes under phpun
- [ ] M7: representative Composer packages pass their test suites
- [ ] M8: Laravel/Symfony bootstrap
- [ ] M9: WordPress executes

Later CLI shape: `phpun run|test|serve|install` — `fmt` (#30) is
implemented.

## Development

```sh
cargo fmt --all && cargo clippy --workspace --all-targets
cargo test --workspace
cargo build --release
```

Rules: compile cleanly, no `unsafe`, never panic on valid PHP, structured
errors, small coherent commits, every change should raise the PHPT pass
count without regressions. Differential-test against reference PHP 8.5
when semantics are unclear; add regression tests for fixed bugs.
