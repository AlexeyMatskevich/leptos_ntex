use super::*;
use lets_expect::lets_expect;

// ----- executor one-shot initialization: regression pins -----------
// Two process-global, monotonic one-shot invariants. The executor state
// is shared across the whole test binary, so these pin *runtime
// independence* and a *stable result pair* (which arm is observed
// depends on test order and cannot be set by a `when`), not a fixed
// value. `any_spawner::ExecutorError` derives no `PartialEq`, so the
// pair is matched with `match_pattern!`, not `equal`. Manual-Red is weak
// for both (the code structurally avoids the failure modes) — these lock
// in the current invariants rather than acting as rich behavioural specs.

// A trivial app whose route walk must not require a ntex arbiter.
#[component]
fn Empty() -> impl IntoView {
    provide_meta_context();
    view! { <h1>"empty"</h1> }
}

// Invariant: `ensure_executor_initialized()` must not depend on a
// running ntex arbiter — SSG and library-mode callers invoke public
// entry points (like `generate_route_list`) without booting a ntex
// runtime. The `lets_expect!`-generated test is a plain `#[test]` (no
// `#[ntex::test]`), so it runs off any ntex system and enforces this.
fn generate_routes_without_a_ntex_runtime() {
    let _routes = gen_route_list(Empty);
    let _routes2 = gen_route_list(Empty);
}

lets_expect! {
    expect(generate_routes_without_a_ntex_runtime()) as executor_initialization {
        to not_panic
    }
}

// Invariant: `try_init_executor()` is idempotent — two consecutive calls
// return the same outcome (both `Ok`, or both `AlreadySet`); they can
// never disagree. The subject is the ATOMIC pair, because the property
// is a relationship between the two calls and `lets_expect` re-runs the
// subject once per `to` block.
lets_expect! {
    expect((crate::try_init_executor(), crate::try_init_executor())) as repeated_executor_init {
        to produces_a_stable_result_pair {
            match_pattern!(
                (Ok(()), Ok(()))
                    | (
                        Err(any_spawner::ExecutorError::AlreadySet),
                        Err(any_spawner::ExecutorError::AlreadySet),
                    )
            ),
            not_match_pattern!((Ok(()), Err(_))),
            not_match_pattern!((Err(_), Ok(()))),
        }
    }
}
