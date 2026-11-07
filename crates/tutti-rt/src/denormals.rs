//! Runs a closure with denormalized floats flushed to zero.
//!
//! Denormals cost tens of cycles per operation on some CPUs, so a decaying
//! reverb tail or a filter settling toward zero can blow an audio block's
//! budget long after it stopped being audible. Flushing them to zero for the
//! duration of a block removes that cliff.
//!
//! On x86_64 this sets FTZ+DAZ in MXCSR; on aarch64, FZ in FPCR. The register
//! saved on entry is written back when the closure returns or unwinds, so the
//! mode never escapes the block: leaving it set would silently change the
//! arithmetic of every other thread that ran afterwards on the same core.
//!
//! ```
//! use tutti_rt::with_no_denormals;
//!
//! fn audio_callback(buffer: &mut [f32]) {
//!     with_no_denormals(|| {
//!         for s in buffer.iter_mut() {
//!             *s *= 0.5;
//!         }
//!     });
//! }
//! # audio_callback(&mut [1.0; 4]);
//! ```

use core::marker::PhantomData;

/// Runs `f` with denormals flushed to zero and returns its result. The FPU
/// mode in force before the call is restored when `f` returns and when it
/// unwinds.
///
/// Denormals cost tens of cycles per operation on some CPUs, so a decaying
/// reverb tail or a filter settling toward zero can blow an audio block's
/// budget long after it stopped being audible. Wrap the span of an audio
/// block in one call.
///
/// On x86_64 it sets FTZ and DAZ in MXCSR; on aarch64, FZ in FPCR. Calls
/// nest: the inner call saves the outer's mode and restores it. On any other
/// architecture it just calls `f`. It is `#[inline]` and costs two register
/// reads and two writes, with no allocation and no syscall.
///
/// The mode is restored by a scope-bound value `f` cannot reach, so it cannot
/// be forgotten, dropped out of order, or moved to another thread. Code in
/// `f` that itself writes MXCSR or FPCR is undone on exit.
///
/// # Soundness
///
/// This function steps outside the floating-point environment Rust compiles
/// for, and tutti accepts that knowingly. The rule:
///
/// - LLVM's LangRef, *Floating-Point Environment*: "Results assume the
///   round-to-nearest rounding mode, and subnormals are assumed to be
///   preserved. Running LLVM code in an environment where these assumptions
///   are not met typically leads to undefined behavior."
/// - `core::arch::x86_64::_mm_setcsr`, deprecated since 1.75: "modifying the
///   masking flags, rounding mode, or denormals-are-zero mode flags leads to
///   **immediate Undefined Behavior**: Rust assumes that these are always in
///   their default state and will optimize accordingly." Its sanctioned
///   alternative is to do the float work inside one `asm!` block and restore
///   MXCSR before that block ends. The Reference's inline-assembly rules say
///   nothing about MXCSR's control bits or FPCR either way (`preserves_flags`
///   covers only status and exception flags), so the rule is the two above.
///
/// `f` is ordinary Rust code that runs with FTZ/DAZ (or FZ) set, which is
/// what those texts call undefined behavior. The closure form does not change
/// that. It fixes one thing: the mode can no longer outlive the call, because
/// the restore is not something `f` or a caller can forget, skip or reorder.
/// The function is still a
/// safe `fn`, by choice rather than by proof: `tutti-graph` is
/// `#![forbid(unsafe_code)]` and calls it. What tutti relies on instead:
///
/// - **The deviation reaches only float values.** The mode changes what an
///   operation returns when an operand or result is subnormal (it becomes
///   ±0), and LLVM, assuming IEEE subnormals, may fold or rewrite such an
///   expression to the unflushed value. Either way the outcome is a float
///   that differs from another float near zero.
/// - **Nothing inside a call makes memory safety depend on an exact float
///   result.** The code run inside is graph nodes and their DSP, the
///   executor's ops, and, in the plugin server, the plugin's own `process`.
///   Audited when this section was written: the DSP crates have no `unsafe`,
///   every float-to-index cast on the audio path is clamped or bounded before
///   it indexes (a safe index, so a miss would be a panic, not a stray
///   access), and the `unsafe` the path does run (this crate's `rt`
///   primitives, the FFT crates, the plugin FFI) indexes with integers from
///   the plan or the buffer layout. The one dependency that indexes unchecked
///   with partly float-derived integers is `libm`'s large-argument reduction
///   for `tan`, whose intermediates are scaled into the normal range, where
///   the mode changes nothing.
///
/// So **do not rely on a bit-exact float result inside the closure** for
/// anything but the audio itself: a value near the subnormal range can differ
/// between a folded and a computed expression, between optimisation levels,
/// and between targets. Within one build, renders still agree bit for bit
/// where they run the same code in the same mode, and the parallel executor's
/// identity with the serial one rests on that: the serial executor wraps the
/// whole block, and in the parallel one every participant (the caller, which
/// is already inside that call, and each helper) makes its own call before
/// its first op, so every node runs, through the same `dyn Node` code, under
/// FTZ/DAZ. The call only adds those bits to the thread's register, so this
/// assumes the other control bits (rounding mode, exception masks) are at
/// their defaults on every participating thread, which Rust requires anyway.
/// The reference interpreter wraps its render the same way.
///
/// # Examples
///
/// ```
/// use tutti_rt::with_no_denormals;
///
/// let mut buffer = [1.0_f32; 4];
/// let sum = with_no_denormals(|| {
///     for s in buffer.iter_mut() {
///         *s *= 0.5;
///     }
///     buffer.iter().sum::<f32>()
/// });
/// assert_eq!(sum, 2.0);
/// ```
#[inline]
pub fn with_no_denormals<R>(f: impl FnOnce() -> R) -> R {
    let _restore = Restore::set();
    f()
}

/// The FPU mode saved by [`Restore::set`], written back on drop. Private, so
/// `f` cannot reach it to forget or reorder it.
struct Restore {
    #[cfg(target_arch = "x86_64")]
    prev_mxcsr: u32,
    #[cfg(target_arch = "aarch64")]
    prev_fpcr: u64,
    /// `!Send` and `!Sync`: the saved register belongs to the thread that
    /// built it, and dropping it on another would write it there.
    _thread: PhantomData<*const ()>,
}

impl Restore {
    /// Sets flush-to-zero and captures the previous FPU mode for the drop.
    #[inline]
    fn set() -> Self {
        #[cfg(target_arch = "x86_64")]
        {
            unsafe {
                let mut prev: u32 = 0;
                core::arch::asm!("stmxcsr [{}]", in(reg) &mut prev, options(nostack));
                let new = prev | 0x8040; // FTZ | DAZ
                core::arch::asm!("ldmxcsr [{}]", in(reg) &new, options(nostack));
                Self {
                    prev_mxcsr: prev,
                    _thread: PhantomData,
                }
            }
        }
        #[cfg(target_arch = "aarch64")]
        {
            unsafe {
                let prev: u64;
                core::arch::asm!("mrs {}, fpcr", out(reg) prev);
                core::arch::asm!("msr fpcr, {}", in(reg) prev | (1 << 24)); // FZ
                Self {
                    prev_fpcr: prev,
                    _thread: PhantomData,
                }
            }
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            Self {
                _thread: PhantomData,
            }
        }
    }
}

impl Drop for Restore {
    #[inline]
    fn drop(&mut self) {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            core::arch::asm!("ldmxcsr [{}]", in(reg) &self.prev_mxcsr, options(nostack));
        }
        #[cfg(target_arch = "aarch64")]
        unsafe {
            core::arch::asm!("msr fpcr, {}", in(reg) self.prev_fpcr);
        }
    }
}

#[cfg(test)]
mod tests {
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
}
