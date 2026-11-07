//! `WorkerPoolBuilder::build` reports a refused thread instead of panicking,
//! and leaves no helper running.
//!
//! Linux only: the refusal is `RLIMIT_NPROC` lowered to the user's current
//! thread count, so the next `spawn` fails with `EAGAIN`. nextest runs each
//! test in its own process, so the lowered limit touches nothing else. The
//! limit counts every thread of the user, so the headroom is read once and
//! the pool asks for far more than that.
//!
//! Mutation (run): `build` unwraps the spawn (a `panic!` on the error) -> the
//! test panics instead of seeing `Err`; `build` skips a failed spawn
//! (`Err(_) => continue`) -> it returns `Ok` and `expect_err` fails.
#![cfg(target_os = "linux")]

use tutti_graph::WorkerPool;

fn user_threads() -> u64 {
    // SAFETY: `getuid` has no preconditions.
    let uid = unsafe { libc::getuid() };
    let mut total = 0;
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(status) = std::fs::read_to_string(entry.path().join("status")) else {
            continue;
        };
        let field = |name: &str| {
            status
                .lines()
                .find_map(|l| l.strip_prefix(name))
                .map(str::trim)
        };
        let owned = field("Uid:")
            .and_then(|u| u.split_whitespace().next())
            .is_some_and(|u| u == uid.to_string());
        if owned {
            total += field("Threads:").and_then(|t| t.parse().ok()).unwrap_or(0);
        }
    }
    total
}

#[test]
fn a_refused_thread_is_an_error_not_a_panic() {
    let now = user_threads();
    let limit = libc::rlimit {
        rlim_cur: now + 2,
        rlim_max: now + 2,
    };
    // SAFETY: `limit` is a valid `rlimit` for the call.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NPROC, &limit) }, 0);
    let refused = WorkerPool::builder()
        .num_threads(64)
        .build()
        .expect_err("64 threads exceed a limit 2 above now");
    assert_eq!(refused.kind(), std::io::ErrorKind::WouldBlock, "EAGAIN");
}
