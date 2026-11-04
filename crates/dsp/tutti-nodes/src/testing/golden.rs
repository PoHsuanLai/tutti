//! Golden digests: asserted where they were recorded, skipped out loud elsewhere.
//!
//! A digest of a render that calls `sin`, `cos`, `exp` or `pow` pins the C
//! runtime's libm, which differs in the last ulp between versions. The
//! goldens were recorded on [`RECORDED_ARCH`] with glibc [`RECORDED_GLIBC`]
//! (Ubuntu 24.04). Any other target cannot say whether a mismatch is a
//! regression, so [`assert_golden`] does not compare there: it prints why on
//! stderr and returns.
//!
//! `TUTTI_GOLDEN` selects the mode:
//!
//! - unset or empty: assert on the recorded target, print a skip note elsewhere.
//! - `require`: assert on the recorded target, and **fail** elsewhere. CI sets
//!   it on its Linux jobs, so a runner image that moves the libm breaks the
//!   build instead of silently asserting nothing.
//! - `record`: never fail on a digest; print every figure as
//!   `GOLDEN <what> <digest>`. To re-record after a runner or toolchain move,
//!   run `TUTTI_GOLDEN=record cargo nextest run --workspace --no-capture` and
//!   read the figures off the output, then update `RECORDED_GLIBC`. Refused
//!   when `CI` is set, so a leftover export cannot turn CI into passes.
//!
//! Any other non-empty value panics, so a typo cannot disable the guard.

/// The glibc `major.minor` the goldens were recorded on.
pub const RECORDED_GLIBC: (u32, u32) = (2, 39);

/// The `target_arch` the goldens were recorded on.
pub const RECORDED_ARCH: &str = "x86_64";

#[cfg(all(target_os = "linux", target_env = "gnu"))]
extern "C" {
    fn gnu_get_libc_version() -> *const std::ffi::c_char;
}

/// The running glibc's `major.minor`, or `None` off glibc.
fn glibc_version() -> Option<(u32, u32)> {
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

#[cfg(any(test, all(target_os = "linux", target_env = "gnu")))]
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

/// The mode a `TUTTI_GOLDEN` value selects; `ci` is whether `CI` is set.
/// Panics on an unknown value, and on `record` under CI.
fn parse_mode(value: Option<&str>, ci: bool) -> Mode {
    match value {
        None | Some("") => Mode::Assert,
        Some("require") => Mode::Require,
        Some("record") => {
            assert!(
                !ci,
                "TUTTI_GOLDEN=record is refused when CI is set: it never fails on a digest, \
                 so a leftover export would turn CI into silent passes"
            );
            Mode::Record
        }
        Some(other) => panic!(
            "TUTTI_GOLDEN={other:?} is not a mode; valid values are \
             \"require\", \"record\", or unset/empty"
        ),
    }
}

fn mode() -> Mode {
    let ci = std::env::var_os("CI").is_some_and(|v| !v.is_empty());
    parse_mode(std::env::var("TUTTI_GOLDEN").ok().as_deref(), ci)
}

/// Assert `got` is the digest recorded as `want`, on the target it was
/// recorded on (see the module docs for the others and `TUTTI_GOLDEN`). The
/// message carries the new figure.
#[track_caller]
pub fn assert_golden(what: &str, got: u64, want: u64) {
    decide(
        what,
        got,
        want,
        mode(),
        std::env::consts::ARCH,
        glibc_version(),
    );
}

#[track_caller]
fn decide(what: &str, got: u64, want: u64, mode: Mode, arch: &str, glibc: Option<(u32, u32)>) {
    if mode == Mode::Record {
        eprintln!("GOLDEN {what} {got:#018x} (recorded {want:#018x})");
        return;
    }
    if arch == RECORDED_ARCH && glibc == Some(RECORDED_GLIBC) {
        assert_eq!(
            got, want,
            "{what}: digest {got:#018x}, recorded {want:#018x}"
        );
        return;
    }
    let here = match glibc {
        Some((major, minor)) => format!("{arch} with glibc {major}.{minor}"),
        None => format!("{arch} with a libm that is not glibc"),
    };
    let note = format!(
        "{what}: golden digest {want:#018x} recorded on {RECORDED_ARCH} with glibc {}.{}; \
         this is {here} (this run's figure: {got:#018x}); \
         TUTTI_GOLDEN=record prints new figures",
        RECORDED_GLIBC.0, RECORDED_GLIBC.1,
    );
    assert!(mode != Mode::Require, "TUTTI_GOLDEN=require: {note}");
    eprintln!("SKIPPED {note}");
}

#[cfg(test)]
mod tests {
    use super::*;

    const REC: Option<(u32, u32)> = Some(RECORDED_GLIBC);

    #[test]
    fn versions_parse_as_major_minor() {
        assert_eq!(parse_version("2.39"), Some((2, 39)));
        assert_eq!(parse_version("2.43.1"), Some((2, 43)));
        assert_eq!(parse_version("garbage"), None);
    }

    /// Mutation (run): making `glibc_version` return `None` on glibc fails
    /// it.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn the_running_glibc_is_found() {
        assert!(glibc_version().is_some());
    }

    /// Mutations (run): mapping `"require"` to `Mode::Assert`, and mapping
    /// `"record"` to `Mode::Assert`, each fail this test.
    #[test]
    fn every_mode_value_parses() {
        assert_eq!(parse_mode(None, false), Mode::Assert);
        assert_eq!(parse_mode(Some(""), false), Mode::Assert);
        assert_eq!(parse_mode(Some("require"), false), Mode::Require);
        assert_eq!(parse_mode(Some("require"), true), Mode::Require);
        assert_eq!(parse_mode(Some("record"), false), Mode::Record);
    }

    /// Mutation (run): mapping the unknown-value arm to `Mode::Assert`
    /// instead of panicking fails all three.
    #[test]
    #[should_panic(expected = "valid values are")]
    fn a_typo_in_the_mode_panics() {
        parse_mode(Some("Require"), false);
    }

    #[test]
    #[should_panic(expected = "valid values are")]
    fn required_is_not_a_mode() {
        parse_mode(Some("required"), true);
    }

    #[test]
    #[should_panic(expected = "valid values are")]
    fn one_is_not_a_mode() {
        parse_mode(Some("1"), false);
    }

    /// Mutation (run): dropping the `!ci` assert fails it.
    #[test]
    #[should_panic(expected = "refused when CI is set")]
    fn record_is_refused_under_ci() {
        parse_mode(Some("record"), true);
    }

    /// Mutation (run): `assert_ne!` for the `assert_eq!` in `decide` fails
    /// this test and `the_recorded_target_passes_a_match`.
    #[test]
    #[should_panic(expected = "digest 0x0000000000000001, recorded 0x0000000000000002")]
    fn the_recorded_target_asserts() {
        decide("x", 1, 2, Mode::Assert, RECORDED_ARCH, REC);
    }

    #[test]
    fn the_recorded_target_passes_a_match() {
        decide("x", 7, 7, Mode::Assert, RECORDED_ARCH, REC);
        decide("x", 7, 7, Mode::Require, RECORDED_ARCH, REC);
    }

    /// Mutation (run): dropping the architecture from `decide`'s condition
    /// fails this test and `require_fails_on_another_architecture`.
    #[test]
    fn another_target_skips_unless_required() {
        decide("x", 1, 2, Mode::Assert, RECORDED_ARCH, Some((2, 43)));
        decide("x", 1, 2, Mode::Assert, RECORDED_ARCH, None);
        decide("x", 1, 2, Mode::Assert, "aarch64", REC);
        decide("x", 1, 2, Mode::Record, RECORDED_ARCH, REC);
    }

    /// Mutation (run): replacing the `Require` assert with `assert!(true)`
    /// fails this test and the two after it.
    #[test]
    #[should_panic(expected = "TUTTI_GOLDEN=require")]
    fn require_fails_on_another_libm() {
        decide("x", 1, 1, Mode::Require, RECORDED_ARCH, Some((2, 43)));
    }

    #[test]
    #[should_panic(expected = "this is aarch64 with glibc 2.39")]
    fn require_fails_on_another_architecture() {
        decide("x", 1, 1, Mode::Require, "aarch64", REC);
    }

    #[test]
    #[should_panic(expected = "recorded on x86_64 with glibc 2.39")]
    fn the_note_names_the_recorded_target() {
        decide("x", 1, 1, Mode::Require, RECORDED_ARCH, None);
    }
}
