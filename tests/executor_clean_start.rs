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

fn clean_start_outcomes() -> (bool, bool, bool) {
    let _routes = leptos_ntex_unofficial::generate_route_list(CleanStartProbeApp);
    (
        any_spawner::Executor::init_custom_executor(leptos_ntex_unofficial::NtexExecutor).is_err(),
        leptos_ntex_unofficial::try_init_executor().is_ok(),
        leptos_ntex_unofficial::try_init_executor().is_ok(),
    )
}

lets_expect::lets_expect! {
    expect(clean_start_outcomes()) as lazy_executor_installation_in_a_fresh_process {
        to installs_and_remains_idempotent { equal((true, true, true)) }
    }
}
