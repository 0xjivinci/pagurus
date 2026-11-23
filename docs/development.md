# Development

[← back to the README](../README.md)

## Development

```sh
cargo test --release          # ~160 tests: learning gate, shell gate, path confinement, schedules, checks, models…
cargo clippy --release --all-targets
cargo fmt                     # 120 columns (rustfmt.toml)
pagurus eval                   # learning evals against your configured model
cargo test --release --features uge    # ~190: adds coding, browser, art, the hub, SQLite FTS5
PAGURUS_DEBUG=1 pagurus run hi  # print the context size and token use (with cache hits) of every model call
scripts/build-pi.sh           # cross-build, emulate ARM1176, size budget
```
