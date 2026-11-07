use super::*;
use std::panic::{AssertUnwindSafe, catch_unwind};

/// The flush bits of the thread's control register, whole register for
/// the comparison.
#[cfg(target_arch = "x86_64")]
const FLUSH: u64 = 0x8040;
#[cfg(target_arch = "aarch64")]
const FLUSH: u64 = 1 << 24;

#[cfg(target_arch = "x86_64")]
fn read_reg() -> u64 {
    let mut val: u32 = 0;
    // SAFETY: `val` is a live `u32`, the four bytes `stmxcsr` stores.
    unsafe { core::arch::asm!("stmxcsr [{}]", in(reg) &mut val, options(nostack)) };
    u64::from(val)
}

#[cfg(target_arch = "aarch64")]
fn read_reg() -> u64 {
    let val: u64;
    // SAFETY: reads FPCR into a register, no memory or flags.
    unsafe { core::arch::asm!("mrs {}, fpcr", out(reg) val) };
    val
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn flushing() -> bool {
    read_reg() & FLUSH == FLUSH
}

// Miri cannot execute x86 SSE intrinsics, and these tests are about
// nothing else: they read and write MXCSR directly. Ignored under miri
// rather than cfg'd out, so `cargo miri test` reports them as skipped
// instead of silently covering a smaller surface than it appears to.
//
// Mutation record (each run, then restored). `Restore::drop` emptied, so
// nothing restores: all five tests fail. The restore moved out of `Drop`
// to a call after `f()`: only `after_a_panic` fails (the register stays
// set after an unwind; `nested_panic` passes because the outer call's own
// restore still runs). The `| 0x8040` removed from `Restore::set`:
// `restores_state`, `nested`, `nested_panic` and the subnormal test fail
// on the inside assertions.
#[test]
#[cfg_attr(miri, ignore = "reads MXCSR; miri has no x86 SSE intrinsics")]
fn restores_state() {
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        let before = read_reg();
        assert!(!flushing());
        let out = with_no_denormals(|| {
            assert!(flushing());
            7
        });
        assert_eq!(out, 7, "the closure's value is returned");
        assert_eq!(before, read_reg());
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    assert_eq!(with_no_denormals(|| 7), 7);
}

#[test]
#[cfg_attr(miri, ignore = "reads MXCSR; miri has no x86 SSE intrinsics")]
fn nested() {
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        let original = read_reg();
        with_no_denormals(|| {
            let outer = read_reg();
            with_no_denormals(|| assert!(flushing()));
            assert!(flushing());
            assert_eq!(outer, read_reg(), "the inner call restores the outer mode");
        });
        assert_eq!(original, read_reg());
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    with_no_denormals(|| with_no_denormals(|| ()));
}

#[test]
#[cfg_attr(miri, ignore = "reads MXCSR; miri has no x86 SSE intrinsics")]
fn after_a_panic() {
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        let before = read_reg();
        let r = catch_unwind(AssertUnwindSafe(|| {
            with_no_denormals(|| {
                assert!(flushing());
                panic!("inside");
            })
        }));
        assert!(r.is_err());
        assert_eq!(before, read_reg());
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    assert!(catch_unwind(|| with_no_denormals(|| panic!("inside"))).is_err());
}

#[test]
#[cfg_attr(miri, ignore = "reads MXCSR; miri has no x86 SSE intrinsics")]
fn nested_panic() {
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        let before = read_reg();
        with_no_denormals(|| {
            let outer = read_reg();
            let r = catch_unwind(AssertUnwindSafe(|| {
                with_no_denormals(|| panic!("inner"));
            }));
            assert!(r.is_err());
            assert_eq!(
                outer,
                read_reg(),
                "the outer mode survives the inner unwind"
            );
            assert!(flushing());
        });
        assert_eq!(before, read_reg());
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    with_no_denormals(|| {
        assert!(catch_unwind(|| with_no_denormals(|| panic!("inner"))).is_err());
    });
}

// Mutation: see the record above; the flush is also what the other
// two mutations break (nothing restored: the product after the call is 0).
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[test]
#[cfg_attr(miri, ignore = "reads MXCSR; miri has no x86 SSE intrinsics")]
fn a_subnormal_result_flushes_only_inside() {
    // The product of the smallest normal and a half is subnormal, so
    // IEEE gives a nonzero value and FTZ gives zero.
    use std::hint::black_box;
    let product = || black_box(f32::MIN_POSITIVE) * black_box(0.5_f32);
    assert!(product() > 0.0, "subnormal results survive by default");
    assert_eq!(with_no_denormals(product), 0.0);
    assert!(product() > 0.0, "and again after the call");
}
