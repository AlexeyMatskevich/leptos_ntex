//! Isolated, own-process check of `try_init_executor()` as the literal FIRST
//! executor-touching call in a fresh process.
//!
//! `try_init_executor()`'s own doc comment documents this exact usage: "Call
//! this explicitly when the app mixes multiple runtimes... and you want to
//! fail fast at startup." That is a distinct entry point from the lazy,
//! `Once`-guarded `ensure_executor_initialized()` reached through
//! `generate_route_list()` and every other rendering/SSG/file-serving call
//! (covered by the sibling `executor_clean_start.rs` binary) — both funnel
//! into the same `init_ntex_executor()`/`EXECUTOR_INIT_STATE` machinery, but
//! only a genuinely fresh process, with `try_init_executor()` itself as the
//! very first caller, exercises this specific documented contract: that a
//! direct `try_init_executor()` call, first, in a clean process, actually
//! performs the install and reports `Ok`, not some stale/cached result.
//!
//! Each `tests/*.rs` file is a separate test binary/process, so this and
//! `executor_clean_start.rs` cannot race each other over the process-global
//! `any_spawner` executor and `EXECUTOR_INIT_STATE`.

#[test]
fn try_init_executor_as_the_first_call_in_a_fresh_process_succeeds() {
    // `any_spawner::ExecutorError` has no `PartialEq`, so assert on `is_ok()`
    // rather than `assert_eq!(.., Ok(()))`.
    assert!(
        leptos_ntex_unofficial::try_init_executor().is_ok(),
        "the first executor install in a clean process, via a direct \
         try_init_executor() call, must succeed (Ok), not report a foreign \
         executor already installed"
    );

    // And it stays idempotent afterwards: a second call is also Ok (this
    // crate installed its own executor, so later calls short-circuit to Ok).
    assert!(
        leptos_ntex_unofficial::try_init_executor().is_ok(),
        "a repeat call after our own clean install must remain Ok"
    );
}
