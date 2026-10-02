# dotdrop-rs

Rust port of [dotdrop](https://github.com/deadc0de6/dotdrop) (v1.17.0): save your
dotfiles once, deploy them everywhere. Same CLI, same config files (YAML and TOML),
same `{{@@ ... @@}}` templates (via `minijinja`).

    cargo build --release   # target/release/dotdrop
    cargo test
    tests-ng/run-all.sh     # the original shell scenarios, run against the Rust binary

Unix only (Linux/macOS).

## Differences from the Python version

- `func_file` / `filter_file` point to Python modules; they are ignored with a warning.
- Saving the config rewrites it with `serde_yaml_ng`: comments and formatting are not preserved.
- `backup` defaults to `true` as documented (the Python code effectively defaulted to off).
- `--version` prints the version only; file type detection uses `file(1)` when installed,
  a NUL-byte check otherwise.
- Template output of booleans/lists follows Python (`True`, `['a']`).

## Status

`tests-ng`: 154/159 scenarios pass here. `func_file`/`filter_file` are unsupported by design;
`import-with-trans`, `install-to-temp` and `update` need `tree`/`xxd`, absent from this environment.
