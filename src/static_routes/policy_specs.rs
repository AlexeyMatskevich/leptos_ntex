use super::*;
use lets_expect::*;
use std::sync::atomic::{AtomicUsize, Ordering};

fn admission(limit: Option<u64>, demand: u64, extra: u64) -> (bool, bool, u64, u64) {
    let pool = PermitPool::new("specimen", limit);
    let first = pool.acquire(demand);
    let accepted = first.is_ok();
    let extra_accepted = pool.acquire(extra).is_ok();
    let during = pool.state.lock().unwrap().used;
    drop(first);
    let after = pool.state.lock().unwrap().used;
    (accepted, extra_accepted, during, after)
}

#[derive(Default)]
struct WakeCount(AtomicUsize);
impl futures::task::ArcWake for WakeCount {
    fn wake_by_ref(wake: &Arc<Self>) {
        wake.0.fetch_add(1, Ordering::SeqCst);
    }
}
fn waiting(cancel: bool) -> (bool, bool, usize) {
    let pool = PermitPool::new("renders", Some(1));
    let held = pool.acquire(1).unwrap();
    let mut pending = Box::pin(PermitFuture::new(pool.clone()));
    let wakes = Arc::new(WakeCount::default());
    let waker = futures::task::waker(wakes.clone());
    let mut context = Context::from_waker(&waker);
    let blocked = pending.as_mut().poll(&mut context).is_pending();
    if cancel {
        drop(pending);
        let registered = pool.state.lock().unwrap().pending.len();
        drop(held);
        let acquired = pool.acquire(1).is_ok();
        (blocked, acquired, registered)
    } else {
        drop(held);
        let woken = wakes.0.load(Ordering::SeqCst) > 0;
        let acquired = pending.as_mut().poll(&mut context).is_ready();
        drop(pending);
        let registered = pool.state.lock().unwrap().pending.len();
        (blocked && woken, acquired, registered)
    }
}

#[cfg(unix)]
#[derive(Clone, Copy)]
enum Binding {
    Initial,
    Repeated,
    Conflict,
    Started,
    OtherRoot,
}
#[cfg(unix)]
fn binding(mode: Binding) -> (bool, bool) {
    crate::tests::run_ntex(async move {
        let root = crate::tests::temp_site_root("static_policy_binding");
        let policy =
            StaticRoutePolicy::open(&*root, StaticStorageLimits::new(), StaticWorkLimits::new())
                .await
                .unwrap();
        let other =
            StaticRoutePolicy::open(&*root, StaticStorageLimits::new(), StaticWorkLimits::new())
                .await
                .unwrap();
        // A private runtime keeps this spec independent of the process runtime.
        let listing = crate::routes::NtexRouteListing {
            runtime: Arc::new(StaticRuntime::default()),
            ..Default::default()
        };
        if matches!(mode, Binding::OtherRoot) {
            // The same runtime already serves a different managed root.
            let elsewhere = crate::tests::temp_site_root("static_policy_binding_other");
            let foreign = StaticRoutePolicy::open(
                &*elsewhere,
                StaticStorageLimits::new(),
                StaticWorkLimits::new(),
            )
            .await
            .unwrap();
            foreign
                .configure_routes(std::slice::from_ref(&listing))
                .unwrap();
            listing.runtime.policy.lock().unwrap().start(&elsewhere);
        } else if !matches!(mode, Binding::Initial) {
            policy
                .configure_routes(std::slice::from_ref(&listing))
                .unwrap();
        }
        if matches!(mode, Binding::Started) {
            listing.runtime.policy.lock().unwrap().start(&root);
        }
        let supplied = if matches!(mode, Binding::Conflict) {
            &other
        } else {
            &policy
        };
        let result = supplied.configure_routes(std::slice::from_ref(&listing));
        let bound = listing.runtime.policy.lock().unwrap().bound(&root);
        let preserved = bound.is_some_and(|bound| Arc::ptr_eq(&bound.0, &policy.0));
        let expected = match mode {
            Binding::Initial | Binding::Repeated | Binding::OtherRoot => result.is_ok(),
            Binding::Conflict => matches!(result, Err(StaticPolicyError::ConflictingPolicy)),
            Binding::Started => matches!(result, Err(StaticPolicyError::AlreadyStarted)),
        };
        (expected, preserved)
    })
}

/// A policy can be opened where no ntex System is running, for example from a
/// build step; the blocking installation then runs inline.
#[cfg(unix)]
fn opened_outside_a_system() -> Result<(), String> {
    let root = crate::tests::temp_site_root("static_policy_no_system");
    futures::executor::block_on(StaticRoutePolicy::open(
        &*root,
        StaticStorageLimits::new(),
        StaticWorkLimits::new(),
    ))
    .map(|_| ())
    .map_err(|error| error.to_string())
}

lets_expect! {
    expect(admission(limit, demand, extra)) as static_permit_admission {
        let limit = None;
        let demand = 64;
        let extra = 1;
        to returns_unlimited_demand_on_drop { equal((true, true, 64, 0)) }
        when capacity_is_limited {
            let limit = Some(2);
            let demand = 2;
            to admits_the_exact_limit_and_refuses_additional_demand { equal((true, false, 2, 0)) }
            when capacity_is_zero {
                let limit = Some(0);
                let demand = 1;
                to refuses_positive_demand { equal((false, false, 0, 0)) }
                when demand_is_zero {
                    let demand = 0;
                    to admits_empty_demand { equal((true, false, 0, 0)) }
                }
            }
        }
        when accounting_would_overflow {
            let demand = u64::MAX;
            to refuses_without_changing_the_held_count { equal((true, false, u64::MAX, 0)) }
        }
    }
    expect(waiting(cancel)) as pending_static_permit {
        let cancel = false;
        to wakes_and_acquires_after_release { equal((true, true, 0)) }
        when the_wait_is_canceled {
            let cancel = true;
            to removes_registration_and_allows_reuse { equal((true, true, 0)) }
        }
    }
}

#[cfg(unix)]
lets_expect! {
    expect(binding(mode)) as static_policy_binding {
        let mode = Binding::Initial;
        to binds_the_shared_runtime { equal((true, true)) }
        when the_same_handle_is_repeated {
            let mode = Binding::Repeated;
            to keeps_the_binding { equal((true, true)) }
        }
        when another_handle_is_supplied {
            let mode = Binding::Conflict;
            to refuses_and_preserves_the_original { equal((true, true)) }
        }
        when the_runtime_has_started {
            let mode = Binding::Started;
            to refuses_late_configuration { equal((true, true)) }
        }
        when the_runtime_already_serves_another_managed_root {
            let mode = Binding::OtherRoot;
            to binds_this_root_independently { equal((true, true)) }
        }
    }
}

fn error_chain() -> (bool, bool, bool) {
    let error = StaticPolicyError::Storage(StaticStorageError::from(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "controlled storage denial",
    )));
    let storage = std::error::Error::source(&error);
    let original = storage
        .and_then(std::error::Error::source)
        .and_then(|source| source.downcast_ref::<std::io::Error>());
    (
        storage.is_some_and(|source| source.is::<StaticStorageError>()),
        original.is_some_and(|source| source.kind() == std::io::ErrorKind::PermissionDenied),
        original.is_some_and(|source| source.to_string() == "controlled storage denial"),
    )
}
lets_expect! {
    expect(error_chain()) as static_policy_error {
        when storage_io_fails {
            to preserves_the_original_io_cause { equal((true, true, true)) }
        }
    }
}

#[cfg(not(unix))]
fn unsupported_platform() -> (bool, usize) {
    crate::tests::run_ntex(async {
        let root = crate::tests::temp_site_root("unsupported_static_policy");
        let result =
            StaticRoutePolicy::open(&*root, StaticStorageLimits::new(), StaticWorkLimits::new())
                .await;
        (
            matches!(
                result,
                Err(StaticPolicyError::Storage(
                    StaticStorageError::UnsupportedPlatform
                ))
            ),
            std::fs::read_dir(&*root).unwrap().count(),
        )
    })
}
#[cfg(not(unix))]
lets_expect! {
    expect(unsupported_platform()) as static_policy_platform {
        when capability_identity_is_unsupported {
            to refuses_installation_without_creating_control_state { equal((true, 0)) }
        }
    }
}

#[cfg(unix)]
lets_expect! {
    expect(opened_outside_a_system()) as static_policy_without_a_system {
        to installs_inline { equal(Ok(())) }
    }
}
