//! The running glibc's version, the one `unsafe` read the golden gate needs.

#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[allow(
    unsafe_code,
    reason = "glibc offers its version only through this C symbol"
)]
unsafe extern "C" {
    fn gnu_get_libc_version() -> *const std::ffi::c_char;
}

/// The running glibc's `major.minor`, or `None` off glibc.
#[allow(unsafe_code, reason = "one call into the extern block above")]
pub(crate) fn glibc_version() -> Option<(u32, u32)> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_as_major_minor() {
        assert_eq!(parse_version("2.39"), Some((2, 39)));
        assert_eq!(parse_version("2.43.1"), Some((2, 43)));
        assert_eq!(parse_version("garbage"), None);
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn the_running_glibc_is_found() {
        assert!(glibc_version().is_some());
    }
}
