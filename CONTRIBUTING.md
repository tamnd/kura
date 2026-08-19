# Contributing

Thanks for taking the time.
This file is short on ceremony and specific about the few things that matter here.

## Before you open a pull request

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI runs the same on Linux, macOS and Windows, on x86-64 and arm64, and then compiles the C example against the built library on each of them.
Running it locally is faster than finding out from a red badge.

## The rules that are not negotiable

**A decoder never trusts its input.** Every function that reads bytes has to return an error for a truncated, corrupt or hostile input rather than panicking, indexing out of bounds or returning a value it invented.
If your change touches a decoder, it needs a test that feeds it its own output cut short at every length.
`posting.rs` and `codec.rs` both have one to copy.

**No unwrap on a decode path.** An `expect` in a test is fine.
An `unwrap` in a function that reads a byte slice is a crash waiting for the one file on one disk that went bad.

**One encoding per value.** If an input can be spelled two ways and both decode, reject the one the encoder would not have produced.
Formats with two spellings grow canonicalisation bugs.

**No panic crosses the FFI boundary.** Every entry point in `kura-ffi` catches, returns a status code, and writes its out parameters before any failure path returns.

**Unsafe carries a reason.** Every unsafe block needs a `// SAFETY:` comment saying which caller obligation makes it sound.
The lint is set to deny, so a block without one does not compile.

## Performance

The primitives here sit under a query path, so a change that makes something ten times slower matters more than one that makes it ten percent faster.

```sh
cargo run --release --example bench
```

That prints a table for the posting list, bitmap, varint and vector paths.
Run it before and after your change and put both in the pull request.
It is deliberately a plain timing loop rather than a benchmark framework: the crate has no dependencies and keeping it that way is worth more than confidence intervals.

## Dependencies

The core crate has none and the FFI crate depends only on the core crate.
This engine gets linked into other people's binaries, so a dependency here is a dependency everywhere.
Adding one is a conversation, not a commit.
If you need an algorithm, it is usually shorter to write the fifty lines than to argue for the crate.

## Style

Write Rust that reads like the standard library.
Names say what a thing is rather than what it is made of, functions that can fail return `Result` with a variant that says what was wrong, and public items are documented with the reason they exist rather than a restatement of the signature.

Comments should say why, not what.
The code already says what it does.
A comment earns its place by recording the thing the next person would otherwise have to rediscover: the reason for a bound, the case that made a check necessary, the alternative that was tried and did not work.

Tests are named as sentences that state the property being held, such as `a_truncated_list_is_an_error_not_a_panic`.
A test name that says what the test does rather than what it protects is a test nobody will understand when it fails in two years.

## The ABI

`include/kura.h` is written by hand and checked in.
If you change a signature, a status code or a struct layout in `kura-ffi`, change the header in the same commit and bump `KURA_ABI_VERSION`.
The C example under `examples/c` is compiled in CI, so a header that drifts from the library fails the build rather than a caller's process.

## Commits and pull requests

Conventional commit prefixes are used for the changelog: `feat`, `fix`, `docs`, `test`, `chore`, `ci`, `perf`, `refactor`.
Keep the subject in the imperative and under about seventy characters.

A pull request should say what changed and why, and what you did to convince yourself it works.
Link the issue it closes.
Small and focused beats large and complete.

## Reporting a security issue

Do not open a public issue.
See [SECURITY.md](SECURITY.md).
