# Contributing

This crate is an unofficial ntex adapter for Leptos. Keep the public Leptos integration contract compatible. Check behavior against
the resolved Leptos and ntex versions before borrowing upstream implementation.

## Local Checks

Run these before opening a PR:

```sh
cargo test --locked
cargo test --locked --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --locked --all-features --no-deps
```

`cargo fmt --all -- --check` is also expected to pass. If you run
`cargo fmt`, check the diff and keep format-only churn separate from behavior
changes when practical.

## Development Notes

- `examples/basic.rs` is the smoke-test app for manual SSR checks:
  `cargo run --example basic`.
- Prefer `register_leptos_routes` for examples and docs. It hides the verbose
  ntex `App<..., Err>` bounds and is the path most users should copy.
- Keep MIME detection, validators and file streaming delegated to `NamedFile`.
  Integration workarounds must name the affected dependency version and carry
  a regression test for the observable HTTP contract. Remove them only when
  that test passes with the corrected dependency.
- `build.rs` probes the resolved `ntex-rt::Runner` return type without running
  the runtime. Keep both unit and panic-result API shapes covered by locked and
  freshly resolved consumers; changing a single trait implementation can pass
  the repository lockfile while breaking downstream builds.
- Server-function payload limits and WebSocket knobs belong in
  `LeptosServerFnConfig`, registered with `App::state`.
- Keep public docs compiling with `RUSTDOCFLAGS="-D warnings"`; this crate uses
  the README as crate-level docs.

## Optional MCP development tools

The wrappers in `scripts/` launch preinstalled executables. They never install
packages at startup. Review and install a specific tool version separately,
then set `CONTEXT7_MCP_BIN` and `GITHUB_MCP_BIN` to its executable path before
starting the editor. The commands must support stdio and the corresponding
environment variable (`CONTEXT7_API_KEY` / `GITHUB_PERSONAL_ACCESS_TOKEN`).
Credentials continue to come from `rbw`.

The Context7 wrapper has the same name as the npm executable, so use an
absolute `CONTEXT7_MCP_BIN` path when `scripts/` comes first in `PATH`.
The previous `@modelcontextprotocol/server-github` package is archived; choose
and maintain the installed GitHub server explicitly. These optional tools are
not dependencies of the Rust library or its release checks.

## Test specifications

Build unit and request/API specifications from observable behavior:

1. List the characteristics that affect the behavior, their distinct states and
   a default state for each characteristic.
2. Identify dependencies between characteristics. Prune impossible combinations
   and combinations with equivalent behavior, and explain each omission.
3. Arrange the remaining cases in a context tree. Put defaults at the root and
   override states in nested contexts; trace each leaf to its expected behavior.
4. Write the specification and run the selected tests. For a regression, perform
   a manual Red check in an isolated copy: restore the defective behavior or
   introduce a focused mutation, confirm that the intended assertion fails,
   then restore the fix and confirm that the same test passes.

A synchronous `lets_expect` subject can run an ntex runtime fixture; do not
switch the application runtime to satisfy a test macro. Keep temporary
directories and other required cleanup under RAII, because `after` does not
execute after a panic.

Keep investigation notes, command logs and temporary mutations outside product
documentation. In PRs, report the exact test commands and results, including
filters that select zero tests and any checks that did not finish. A listing,
compilation, timeout or successful stress run is not evidence that a particular
regression was exercised.

## Release Checklist

1. Update `CHANGELOG.md` for the actual public changes and known limitations.
2. Run the local checks above on the declared Rust 1.88 MSRV and current stable.
3. Execute every combination of `cookie`, `tracing` and `islands-router`, including
   no features. CI executes each combination with both default Neon and
   `ntex/tokio` on Linux. Test the same runtime axis locally; `--all-features`
   alone enables only this adapter's features. Verify other intended platforms
   and runtimes separately and record any gaps.
4. Build and run a separate consumer without this repository's dev-dependencies.
   Check both the release lockfile and a fresh dependency resolution; dev feature
   unification must not hide missing runtime dependencies.
5. Run `cargo audit` with a current RustSec database. Record its revision/date and
   review informational soundness and maintenance advisories as well as reported
   vulnerabilities. Inspect new dependency licenses and sources.
6. Confirm README examples and all `examples/` targets compile. Check hydration
   and client navigation in a browser when their integration changes.
7. Verify normal rustdoc and the docs.rs `--cfg docsrs` configuration. The latter
   requires a suitable nightly; a local emulation is not a remote docs.rs build.
8. Inspect `cargo package --locked --list`, then run `cargo package --locked` to
   verify the archive without publishing. Review included files and public API
   compatibility against the previous release.
9. Re-run relevant cancellation, concurrent publication and real TCP regressions
   after dependency updates. Compare performance under the same bounded workload
   when changing rendering, storage or connection buffering.
