# Contributing

This crate is an unofficial ntex adapter for Leptos. Keep changes close to
the upstream Leptos adapter shape unless ntex needs a different primitive.

## Local Checks

Run these before opening a PR:

```sh
cargo test
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --all-features --no-deps
```

`cargo fmt --all -- --check` is also expected to pass. If you run
`cargo fmt`, check the diff and keep format-only churn separate from behavior
changes when practical.

## Development Notes

- `examples/basic.rs` is the smoke-test app for manual SSR checks:
  `cargo run --example basic`.
- Prefer `register_leptos_routes` for examples and docs. It hides the verbose
  ntex `App<..., Err>` bounds and is the path most users should copy.
- Do not duplicate HTTP behavior that `ntex` or `ntex-files::NamedFile`
  already provides, such as MIME detection, validators, range requests, and
  conditional responses.
- Server-function payload limits and WebSocket knobs belong in
  `LeptosServerFnConfig`, registered with `App::state`.
- Keep public docs compiling with `RUSTDOCFLAGS="-D warnings"`; this crate uses
  the README as crate-level docs.

## Release Checklist

1. Update `CHANGELOG.md`.
2. Verify the local checks above.
3. Confirm README examples and `examples/basic.rs` still match the public API.
4. Confirm docs.rs metadata in `Cargo.toml` still builds all feature docs.
