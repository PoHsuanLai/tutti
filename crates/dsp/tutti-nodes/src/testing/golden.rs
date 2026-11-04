//! Golden digests: asserted where they were recorded, skipped out loud elsewhere.
//!
//! A digest of a render that calls `sin`, `cos`, `exp` or `pow` pins the C
//! runtime's libm, which differs in the last ulp between versions. The
//! goldens were recorded on glibc [`RECORDED_GLIBC`] (Ubuntu 24.04). A runtime
//! that is not that one cannot say whether a mismatch is a regression, so
//! [`assert_golden`] does not compare there: it prints why on stderr and
//! returns.
//!
//! `TUTTI_GOLDEN` selects the mode:
//!
//! - unset: assert on the recorded libm, print a skip note elsewhere.
//! - `require`: assert on the recorded libm, and **fail** elsewhere. CI sets it
//!   on its Linux jobs, so a runner image that moves the libm breaks the build
//!   instead of silently asserting nothing.
//! - `record`: never fail on a digest; print every figure as
//!   `GOLDEN <what> <digest>`. To re-record after a runner or toolchain move,
//!   run `TUTTI_GOLDEN=record cargo nextest run --workspace --no-capture` and
//!   read the figures off the output, then update `RECORDED_GLIBC`.

/// The glibc `major.minor` the goldens were recorded on.
pub const RECORDED_GLIBC: (u32, u32) = (2, 39);

#[cfg(all(target_os = "linux", target_env = "gnu"))]
extern "C" {
    fn gnu_get_libc_version() -> *const std::ffi::c_char;
}

/// The running glibc's `major.minor`, or `None` off glibc.
pub fn glibc_version() -> Option<(u32, u32)> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: glibc returns a pointer to a static NUL-terminated string.
        let s = unsafe { std::ffi::CStr::from_ptr(gnu_get_libc_version()) };
        parse_version(s.to_str().ok()?)
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        None
    }
}

fn parse_version(s: &str) -> Option<(u32, u32)> {
    let mut parts = s.split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Assert,
    Require,
    Record,
}

fn mode() -> Mode {
    match std::env::var("TUTTI_GOLDEN").as_deref() {
        Ok("record") => Mode::Record,
        Ok("require") => Mode::Require,
        _ => Mode::Assert,
    }
}

/// Whether the digests were recorded on the libm this process runs.
pub fn golden_here() -> bool {
    glibc_version() == Some(RECORDED_GLIBC)
}

/// Assert `got` is the digest recorded as `want`, on the libm it was recorded
/// on (see the module docs for the other libms and `TUTTI_GOLDEN`). The
/// message carries the new figure.
#[track_caller]
pub fn assert_golden(what: &str, got: u64, want: u64) {
    decide(what, got, want, mode(), glibc_version());
}

#[track_caller]
fn decide(what: &str, got: u64, want: u64, mode: Mode, glibc: Option<(u32, u32)>) {
    if mode == Mode::Record {
        eprintln!("GOLDEN {what} {got:#018x} (recorded {want:#018x})");
        return;
    }
    if glibc == Some(RECORDED_GLIBC) {
        assert_eq!(
            got, want,
            "{what}: digest {got:#018x}, recorded {want:#018x}"
        );
        return;
    }
    let here = match glibc {
        Some((major, minor)) => format!("glibc {major}.{minor}"),
        None => "a libm that is not glibc".to_owned(),
    };
    let note = format!(
        "{what}: golden digest {want:#018x} recorded on glibc {}.{}; this is {here} \
         (this run's figure: {got:#018x}); TUTTI_GOLDEN=record prints new figures",
        RECORDED_GLIBC.0, RECORDED_GLIBC.1,
    );
    assert!(mode != Mode::Require, "TUTTI_GOLDEN=require: {note}");
    eprintln!("SKIPPED {note}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_as_major_minor() {
        assert_eq!(parse_version("2.39"), Some((2, 39)));
        assert_eq!(parse_version("2.43.1"), Some((2, 43)));
        assert_eq!(parse_version("garbage"), None);
    }

    /// Mutation (run): comparing in the `Assert` arm only when `got != want`
    /// fails the first test; dropping the `Require` assert fails the third.
    #[test]
    #[should_panic(expected = "digest 0x0000000000000001, recorded 0x0000000000000002")]
    fn the_recorded_libm_asserts() {
        decide("x", 1, 2, Mode::Assert, Some(RECORDED_GLIBC));
    }

    #[test]
    fn another_libm_skips_unless_required() {
        decide("x", 1, 2, Mode::Assert, Some((2, 43)));
        decide("x", 1, 2, Mode::Assert, None);
        decide("x", 1, 2, Mode::Record, Some(RECORDED_GLIBC));
    }

    #[test]
    #[should_panic(expected = "TUTTI_GOLDEN=require")]
    fn require_fails_on_another_libm() {
        decide("x", 1, 1, Mode::Require, Some((2, 43)));
    }
}
