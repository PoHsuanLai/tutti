//! RAII guard that flushes denormalized floats to zero.
//!
//! Denormals cost tens of cycles per operation on some CPUs, so a decaying
//! reverb tail or a filter settling toward zero can blow an audio block's
//! budget long after it stopped being audible. Flushing them to zero for the
//! duration of a block removes that cliff.
//!
//! On x86_64 this sets FTZ+DAZ in MXCSR; on aarch64, FZ in FPCR. The previous
//! register state is restored on drop, so the mode never escapes the block —
//! leaving it set would silently change the arithmetic of every other thread
//! that ran afterwards on the same core.
//!
//! ```
//! use tutti_types::ScopedNoDenormals;
//!
//! fn audio_callback(buffer: &mut [f32]) {
//!     let _guard = ScopedNoDenormals::new();
//! }
//! ```

use core::marker::PhantomData;

/// An RAII guard that flushes denormals to zero while it is alive, restoring
/// the previous FPU mode on drop.
///
/// Denormals cost tens of cycles per operation on some CPUs, so a decaying
/// reverb tail or a filter settling toward zero can blow an audio block's
/// budget long after it stopped being audible. Hold one guard for the span of
/// an audio block: construct it at the top of the callback and let it drop at
/// the end.
///
/// On x86_64 it sets FTZ and DAZ in MXCSR; on aarch64, FZ in FPCR. The previous
/// register state is restored on drop, so the mode never leaks into other code
/// that later runs on the same thread. Guards nest. On any other architecture
/// it is an empty struct and every operation compiles away.
///
/// # Soundness
///
/// This guard steps outside the floating-point environment Rust compiles
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
/// This guard leaves FTZ/DAZ (or FZ) set across ordinary Rust code until it
/// drops, which is what those texts call undefined behavior. `new` is still
/// a safe `fn`, by choice rather than by proof: `tutti-graph` is
/// `#![forbid(unsafe_code)]` and calls it. What tutti relies on instead:
///
/// - **The deviation reaches only float values.** The mode changes what an
///   operation returns when an operand or result is subnormal (it becomes
///   ±0), and LLVM, assuming IEEE subnormals, may fold or rewrite such an
///   expression to the unflushed value. Either way the outcome is a float
///   that differs from another float near zero.
/// - **Nothing in a guarded block makes memory safety depend on an exact
///   float result.** The code under the guards is graph nodes and their
///   DSP, the executor's ops, and, in the plugin server, the plugin's own
///   `process`. Audited when this section was written: the DSP crates have no
///   `unsafe`, every float-to-index cast on the audio path is clamped or
///   bounded before it indexes (a safe index, so a miss would be a panic,
///   not a stray access), and the `unsafe` the path does run (this crate's
///   `rt` primitives, the FFT crates, the plugin FFI) indexes with integers
///   from the plan or the buffer layout. The one dependency that indexes
///   unchecked with partly float-derived integers is `libm`'s large-argument
///   reduction for `tan`, whose intermediates are scaled into the normal
///   range, where the mode changes nothing.
/// - **The mode is scoped.** It is set in `new` and the saved register is
///   written back in `drop`, unwinding included. The guard is `!Send` and
///   `!Sync`, so it cannot be moved to another thread and dropped there.
///   Drop guards in reverse order of creation (the natural order for
///   locals) and never [`forget`](core::mem::forget) one: either leaves the
///   wrong mode on the thread.
///
/// So **do not rely on a bit-exact float result inside a guard** for anything
/// but the audio itself: a value near the subnormal range can differ between a
/// folded and a computed expression, between optimisation levels, and between
/// targets. Within one build, renders still agree bit for bit where they run
/// the same code in the same mode, and the parallel executor's identity with
/// the serial one rests on that: the serial executor holds a guard for the
/// whole block, and in the parallel one every participant (the caller, which
/// is already inside that guard, and each helper) builds its own guard before
/// its first op, so every node runs, through the same `dyn Node` code, under
/// FTZ/DAZ. The guard only adds those bits to the thread's register, so this
/// assumes the other control bits (rounding mode, exception masks) are at
/// their defaults on every participating thread, which Rust requires anyway.
/// The reference interpreter guards its render the same way.
///
/// # Examples
///
/// ```
/// use tutti_types::ScopedNoDenormals;
///
/// fn audio_callback(buffer: &mut [f32]) {
///     let _guard = ScopedNoDenormals::new();
///     for s in buffer.iter_mut() {
///         *s *= 0.5;
///     }
/// }
/// # audio_callback(&mut [1.0; 4]);
/// ```
///
/// A guard cannot leave its thread:
///
/// ```compile_fail
/// use tutti_types::ScopedNoDenormals;
///
/// let guard = ScopedNoDenormals::new();
/// std::thread::spawn(move || drop(guard));
/// ```
// Mutation: with the `_thread` field removed the guard is `Send` again, and
// this `compile_fail` doctest compiles, so `cargo test --doc` fails it.
pub struct ScopedNoDenormals {
    #[cfg(target_arch = "x86_64")]
    prev_mxcsr: u32,
    #[cfg(target_arch = "aarch64")]
    prev_fpcr: u64,
    /// `!Send` and `!Sync`: the saved register belongs to the thread that
    /// built the guard, and dropping it on another would write it there.
    _thread: PhantomData<*const ()>,
}

impl ScopedNoDenormals {
    /// Sets flush-to-zero and captures the previous FPU mode for the drop.
    ///
    /// Cheap enough for the audio thread: two register accesses, no allocation
    /// and no syscall.
    #[inline]
    pub fn new() -> Self {
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

impl Default for ScopedNoDenormals {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ScopedNoDenormals {
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

    #[cfg(target_arch = "x86_64")]
    unsafe fn read_mxcsr() -> u32 {
        let mut val: u32 = 0;
        core::arch::asm!("stmxcsr [{}]", in(reg) &mut val, options(nostack));
        val
    }

    // Miri cannot execute x86 SSE intrinsics, and these two tests are about
    // nothing else: they read and write MXCSR directly. Ignored under miri
    // rather than cfg'd out, so `cargo miri test` reports them as skipped
    // instead of silently covering a smaller surface than it appears to.
    #[test]
    #[cfg_attr(miri, ignore = "reads MXCSR; miri has no x86 SSE intrinsics")]
    fn test_restores_state() {
        #[cfg(target_arch = "x86_64")]
        {
            let before = unsafe { read_mxcsr() };
            {
                let _guard = ScopedNoDenormals::new();
                assert_ne!(unsafe { read_mxcsr() } & 0x8040, 0);
            }
            assert_eq!(before, unsafe { read_mxcsr() });
        }
        #[cfg(target_arch = "aarch64")]
        {
            let before: u64;
            unsafe { core::arch::asm!("mrs {}, fpcr", out(reg) before) };
            {
                let _guard = ScopedNoDenormals::new();
                let during: u64;
                unsafe { core::arch::asm!("mrs {}, fpcr", out(reg) during) };
                assert_ne!(during & (1 << 24), 0);
            }
            let after: u64;
            unsafe { core::arch::asm!("mrs {}, fpcr", out(reg) after) };
            assert_eq!(before, after);
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            let _guard = ScopedNoDenormals::new();
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "reads MXCSR; miri has no x86 SSE intrinsics")]
    fn test_nested() {
        #[cfg(target_arch = "x86_64")]
        {
            let original = unsafe { read_mxcsr() };
            {
                let _outer = ScopedNoDenormals::new();
                {
                    let _inner = ScopedNoDenormals::new();
                    assert_ne!(unsafe { read_mxcsr() } & 0x8040, 0);
                }
                assert_ne!(unsafe { read_mxcsr() } & 0x8040, 0);
            }
            assert_eq!(original, unsafe { read_mxcsr() });
        }
        #[cfg(target_arch = "aarch64")]
        {
            let original: u64;
            unsafe { core::arch::asm!("mrs {}, fpcr", out(reg) original) };
            {
                let _outer = ScopedNoDenormals::new();
                {
                    let _inner = ScopedNoDenormals::new();
                }
            }
            let after: u64;
            unsafe { core::arch::asm!("mrs {}, fpcr", out(reg) after) };
            assert_eq!(original, after);
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            let _outer = ScopedNoDenormals::new();
            let _inner = ScopedNoDenormals::new();
        }
    }
}
