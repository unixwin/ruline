# Acknowledgements

ruline is independent, but it did not appear from nothing. We are grateful to
the projects and people below, and we keep their notices intact.

## reedline and the Nushell Project

ruline started from [nushell/reedline](https://github.com/nushell/reedline)
0.52.1. The initial editor architecture, the Rust API shape, the examples and
a large part of the test suite come from that code base.

- License: MIT
- Copyright (c) 2021 Nushell Project
- Thank you to the Nushell Project and every reedline contributor, past and
  present. Your work made this project possible.

The full MIT text and the original copyright line are retained in
[`LICENSE`](./LICENSE). ruline is now an independent project — we do not track
or merge reedline — so bugs and design choices after the starting point are
ours, not theirs.

## GNU readline

The compatibility target is GNU readline 8.x. The behavioural specification is
the vendored bash 5.3.20 tree in
[unixwin/rubash](https://github.com/unixwin/rubash)
(`third_party/bash/lib/readline`), which carries GNU readline's own license.

- We do **not** copy GNU readline code or headers into this repository.
- The `readline-abi` crate uses clean-room C declarations for the binary
  interface; the ABI symbol inventory contains names only.
- Thank you to Chet Ramey and the GNU readline contributors for decades of
  work defining the interactive line-editing contract. Compatibility work is
  much easier when the target is that well documented.

## Rust ecosystem

ruline is built with [crossterm](https://github.com/crossterm-rs/crossterm),
[nu-ansi-term](https://github.com/nushell/nu-ansi-term), serde, unicode
segmentation/width crates and the rest of the dependency tree recorded in
`Cargo.lock`. Thank you to their maintainers.

## A note on licenses

ruline is MIT licensed. This repository contains no GPL code: GNU readline is
a behavioural reference, and the C ABI layer is a clean-room declaration of
the public interface. If you believe an attribution is missing, please open an
issue and we will fix it.
