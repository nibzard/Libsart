# Fuzz testing

These targets exercise in-memory code. They do not modify writing projects or
invoke Git. The separate Cargo workspace keeps fuzz dependencies out of the
application's lockfile.

- `markdown` checks line offsets and block ranges for arbitrary UTF-8 text.
- `wrapping` checks source anchors and preservation of every character except
  ASCII spaces, which wrapping can remove. Its first input byte selects a
  width from 0 through 255. The remaining bytes must be valid UTF-8 text.

The checked-in seeds include Markdown structures, incomplete syntax, Unicode,
control characters, and spaces. Generated inputs and failure artifacts are
ignored by Git. Keep `Cargo.lock` tracked to reproduce dependency versions.

## Run a short check

Run these commands from the repository root:

1. Install the tools:

   ```sh
   rustup toolchain install nightly --profile minimal
   cargo install cargo-fuzz --locked
   ```

2. Copy the seeds into the writable corpora. Repeat this step after new seeds
   are added:

   ```sh
   mkdir -p fuzz/corpus/markdown fuzz/corpus/wrapping
   cp fuzz/seeds/markdown/* fuzz/corpus/markdown/
   cp fuzz/seeds/wrapping/* fuzz/corpus/wrapping/
   ```

3. Run each target for about 30 seconds after compilation:

   ```sh
   cargo +nightly fuzz run markdown -- -max_total_time=30 -max_len=16384 -timeout=10
   cargo +nightly fuzz run wrapping -- -max_total_time=30 -max_len=16384 -timeout=10
   ```

For longer local runs, increase `-max_total_time`. The input size limit bounds
individual cases; larger inputs need a separate run. A short run is a smoke
check, not proof that the code has no defects. These checks complement the
existing generated property tests and formal model checks.

## Investigate a failure

The fuzzer prints the artifact path. Replay and minimize it with:

```sh
cargo +nightly fuzz run markdown fuzz/artifacts/markdown/<artifact>
cargo +nightly fuzz tmin markdown fuzz/artifacts/markdown/<artifact>
```

Use `wrapping` in both paths for a wrapping failure. Check whether the failure
comes from the application or an incorrect assertion in the target. Add a
regression test and a small seed when fixing a confirmed defect.

The wrapping target does not require exact space preservation. It also does
not assert a strict display width: a single wide character can exceed a
one-cell row. Neither target exercises editor actions or Git operations.

See the [Rust Fuzz Book](https://rust-fuzz.github.io/book/cargo-fuzz.html) for
coverage reports and additional options.
