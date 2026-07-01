//! Isolated, own-process check of the executor clean-start branch — the lazy,
//! `Once`-guarded `ensure_executor_initialized()` reached through every
//! rendering/SSG/file-serving entry point (public surface: `generate_route_list`
//! and friends).
//!
//! Each `tests/*.rs` file is a SEPARATE test binary, so this process is
//! guaranteed to start with `ExecutorInitState::Unknown` and no competing
//! `any_spawner` executor. The sibling binary
//! `executor_direct_init_clean_start.rs` covers the OTHER first-caller entry
//! point — a direct `try_init_executor()` call, first, in its own fresh
//! process — in its own process, so the two documented entry points never
//! race each other over the process-global executor state (see that file's
//! doc comment).
//!
//! The in-crate `repeated_executor_init` spec cannot pin this branch either:
//! it shares the main test binary, whose executor state is already installed
//! (and unknowable) by the time it runs, so it has to accept the
//! `(Err(AlreadySet), ...)` pair. This binary closes that gap by observing
//! the lazy installer directly, in a process where it is the only thing that
//! touches the executor.

use leptos::prelude::*;
use leptos_meta::provide_meta_context;

#[component]
fn CleanStartProbeApp() -> impl IntoView {
    provide_meta_context();
    view! { <h1>"clean-start probe"</h1> }
}

#[test]
fn first_executor_init_in_a_fresh_process_succeeds() {
    // The lazy, `Once`-guarded path (`ensure_executor_initialized`) is what
    // every real app actually hits first — through `generate_route_list` and
    // every other rendering/SSG/file-serving entry point, never through
    // `try_init_executor()` directly. Calling the public
    // `generate_route_list` here, as the very first executor-touching call
    // in this fresh process, exercises that lazy install and confirms it
    // does not panic.
    let _routes = leptos_ntex_unofficial::generate_route_list(CleanStartProbeApp);

    // Confirm the lazy install actually performed a clean, successful
    // install (not a silently-swallowed error): `any_spawner::ExecutorError`
    // has no `PartialEq`, so assert on `is_ok()` rather than
    // `assert_eq!(.., Ok(()))`. Because this crate's own executor already
    // won the process-global race above, this call takes the cached
    // `NtexInstalled` short-circuit and must still report `Ok`.
    assert!(
        leptos_ntex_unofficial::try_init_executor().is_ok(),
        "ensure_executor_initialized(), reached via generate_route_list() as \
         the first executor-touching call in a clean process, must have \
         performed a successful install — a later try_init_executor() must \
         report Ok, not a foreign executor already installed"
    );

    // And it stays idempotent afterwards: a further repeat call is also Ok.
    assert!(
        leptos_ntex_unofficial::try_init_executor().is_ok(),
        "a repeat call after our own clean install must remain Ok"
    );
}
