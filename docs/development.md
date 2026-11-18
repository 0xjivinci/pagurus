# Development

[← back to the README](../README.md)

## Development

```sh
cargo test --release          # 141 tests: learning gate, shell gate, path confinement, schedules, checks, models…
cargo clippy --release
pagurus eval                   # learning evals against your configured model
cargo test --release --features full   # the same with SQLite FTS5
PAGURUS_DEBUG=1 pagurus run hi  # print the context size and token use (with cache hits) of every model call
scripts/build-pi.sh           # cross-build, emulate ARM1176, size budget
```
