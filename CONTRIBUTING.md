# Contributing

ruline is an independent project: a GNU readline-compatible line editor for
niubash and for C readline consumers. It started from nushell/reedline 0.52.1
(MIT) and does not track that project. Contributions are welcome from anyone
building shells, REPLs or readline-compatible tooling.

Feel free to open an [issue](https://github.com/unixwin/ruline/issues) or a
discussion. The compatibility tracker is
[unixwin/ruline#8](https://github.com/unixwin/ruline/issues/8); the GNU source
vendored in `unixwin/rubash` is the behavioural oracle.

## Good starting points

Check the [issues](https://github.com/unixwin/ruline/issues). The readline ABI
phases are labelled in the tracker and are the best entry points right now.

## Developing

### Set up

This is no different than other Rust projects.

```bash
git clone https://github.com/unixwin/ruline
cd ruline
# To try our example program
cargo run --example basic
```

### Code style

We follow the standard rust formatting style and conventions suggested by [clippy](https://github.com/rust-lang/rust-clippy).

### To make the CI gods happy

> The commands below reflect what CI runs. Not all flags are always necessary for local development (e.g. feel free to omit `--all-features` or `--all-targets` during iteration), but running these exact commands before opening a PR ensures you match CI behavior.

Before opening a PR, run these locally:

- Format:

  ```shell
  cargo fmt --all
  ```

  > `--all` formats all packages in the workspace.

- Lint (Clippy):

  ```shell
  cargo clippy --locked --all-targets --all-features
  ```

  > `--all-targets` checks lib, bins, tests, examples, and benches. 
  > 
  > `--all-features` enables all optional features. 
  >
  > `--locked` ensures `Cargo.lock` is in sync with `Cargo.toml`.
  
  > Note: CI treats Clippy warnings as errors.



- Tests:

> CI uses [cargo-nextest](https://nexte.st/) for its test runner. See the [installation instructions](https://nexte.st/docs/installation/pre-built-binaries/).

  ```shell
  cargo nextest run --all --all-features
  ```

