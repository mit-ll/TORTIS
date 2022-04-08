## Building

```bash
cargo +stage1 build
```

## Running tests

Run tests with `TXN=true cargo +stage1 test`.

The `RUSTC_LOG` environment variable sets the log level per package with
`RUSTC_LOG=$package=$level`, e.g.
`RUSTC_LOG=rustc_mir::transform::transaction=debug`.
The log levels are `warn`, `debug`, and `info`.

Output logs to a file with `|& tee out.log`.

Get backtrace information on panic with `RUST_BACKTRACE=1`.

All together:

```bash
RUSTC_LOG=rustc_mir::transform::transaction=debug TXN=true RUST_BACKTRACE=1 cargo +stage1 test
```

The test binaries are cached, so if you rebuild the compiler you'll need to rebuild the binary manually. I usually do `cargo clean` and then `cargo +stage1 test` again, or specifically delete my desired binary at `target/debug/deps/test-name-012345` where 012345 is some hash, then rebuild.

## Performance Evaluation

Get test data by enabling debug output with `-- --nocapture`.

```bash
TXN=true cargo +stage1 test linear -- --nocapture
```

## Viewing MIR

`.cargo/config` ensures that `cargo` commands emit MIR by default.

MIR will be located in:

```bash
target/debug/deps/$filename-$hash.mir
```
