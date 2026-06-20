//! Isolated, own-process check of the executor clean-start branch.
//!
//! Each `tests/*.rs` file is a SEPARATE test binary, so this process is
//! guaranteed to start with `ExecutorInitState::Unknown` and no competing
//! `any_spawner` executor. The FIRST `try_init_executor()` must therefore take
//! the clean-install path and succeed.
//!
//! The in-crate `repeated_executor_init` spec cannot pin this: it shares the
//! main test binary, whose executor state is already installed (and unknowable)
//! by the time it runs, so it has to accept the `(Err(AlreadySet), ...)` pair.
//! This binary closes that gap by observing the clean install directly, in a
//! process where it is the only thing that touches the executor.

#[test]
fn first_executor_init_in_a_fresh_process_succeeds() {
    // `any_spawner::ExecutorError` has no `PartialEq`, so assert on `is_ok()`
    // rather than `assert_eq!(.., Ok(()))`.
    assert!(
        leptos_ntex_unofficial::try_init_executor().is_ok(),
        "the first executor install in a clean process must succeed (Ok), \
         not report a foreign executor already installed"
    );

    // And it stays idempotent afterwards: a second call is also Ok (this crate
    // installed its own executor, so later calls short-circuit to Ok).
    assert!(
        leptos_ntex_unofficial::try_init_executor().is_ok(),
        "a repeat call after our own clean install must remain Ok"
    );
}
