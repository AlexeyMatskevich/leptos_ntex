// ntex-rt 3.17 added a panic result to Runner::block_on. Probe the resolved
// trait's return type without running it, so both API shapes remain supported.
trait RunnerResult {
    const FALLIBLE: bool;
}
impl RunnerResult for () {
    const FALLIBLE: bool = false;
}
impl RunnerResult for std::thread::Result<()> {
    const FALLIBLE: bool = true;
}
fn is_fallible<T: RunnerResult>(
    _: impl FnOnce(&dyn ntex_rt::Runner, ntex_rt::BlockFuture) -> T,
) -> bool {
    T::FALLIBLE
}
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-check-cfg=cfg(ntex_runner_returns_result)");
    if is_fallible(|runner, future| runner.block_on(future)) {
        println!("cargo:rustc-cfg=ntex_runner_returns_result");
    }
}
