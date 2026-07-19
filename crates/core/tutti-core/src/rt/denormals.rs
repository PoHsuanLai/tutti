//! RAII guard that flushes denormalized floats to zero.
//!
//! On x86_64: sets FTZ+DAZ in MXCSR. On aarch64: sets FZ in FPCR.
//! Previous state is restored on drop.
//!
//! ```
//! use tutti_core::ScopedNoDenormals;
//!
//! fn audio_callback(buffer: &mut [f32]) {
//!     let _guard = ScopedNoDenormals::new();
//! }
//! ```

pub struct ScopedNoDenormals {
    #[cfg(target_arch = "x86_64")]
    prev_mxcsr: u32,
    #[cfg(target_arch = "aarch64")]
    prev_fpcr: u64,
}

impl ScopedNoDenormals {
    #[inline]
    pub fn new() -> Self {
        #[cfg(target_arch = "x86_64")]
        {
            unsafe {
                let mut prev: u32 = 0;
                core::arch::asm!("stmxcsr [{}]", in(reg) &mut prev, options(nostack));
                let new = prev | 0x8040; // FTZ | DAZ
                core::arch::asm!("ldmxcsr [{}]", in(reg) &new, options(nostack));
                Self { prev_mxcsr: prev }
            }
        }
        #[cfg(target_arch = "aarch64")]
        {
            unsafe {
                let prev: u64;
                core::arch::asm!("mrs {}, fpcr", out(reg) prev);
                core::arch::asm!("msr fpcr, {}", in(reg) prev | (1 << 24)); // FZ
                Self { prev_fpcr: prev }
            }
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            Self {}
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

    #[test]
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
