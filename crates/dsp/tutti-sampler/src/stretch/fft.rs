//! The vocoder's fixed-size FFT: `microfft`, dispatched on the window length.
//!
//! `microfft` is allocation-free and in-place, which is what an audio-thread
//! STFT needs, but it spells each length as its own function (`rfft_1024`,
//! `ifft_1024`, …). These two functions are the dispatch from a runtime
//! [`FftSize`](super::FftSize) to that family.
//!
//! They used to be `fundsp-tutti`'s `fft` module, reached through
//! `tutti_core::{real_fft, inverse_fft, Complex32}`. The vocoder was the only
//! caller anywhere in the engine, so the wrapper moved to its one user and the
//! sampler names `microfft` and `num-complex` directly (design doc 013, Phase
//! 0b). Same crates, same versions, same functions — so the transform is bit
//! for bit what it was, which the stretch suite's round-trip test pins.
//!
//! The dispatch covers [`FftSize::MIN`](super::FftSize::MIN) to
//! [`FftSize::MAX`](super::FftSize::MAX) — every length an `FftSize` can hold,
//! and no other. `microfft`'s own floor is 2, but a 2-point window has no `/4`
//! hop, so `FftSize` never produces it.

use microfft::inverse::{
    ifft_1024, ifft_128, ifft_16, ifft_16384, ifft_2048, ifft_256, ifft_32, ifft_32768, ifft_4,
    ifft_4096, ifft_512, ifft_64, ifft_8, ifft_8192,
};
use microfft::real::{
    rfft_1024, rfft_128, rfft_16, rfft_16384, rfft_2048, rfft_256, rfft_32, rfft_32768, rfft_4,
    rfft_4096, rfft_512, rfft_64, rfft_8, rfft_8192,
};
pub(crate) use num_complex::Complex32;

/// Real-valued FFT, in place.
///
/// `data.len()` must be a power of two in `4..=32768`. Returns `data` viewed
/// as `len / 2` complex bins; the Nyquist bin is packed into the imaginary part
/// of DC (bin 0), which the caller unpacks.
///
/// # Panics
///
/// On any other length. `FftSize::new` refuses those before a vocoder exists,
/// so reaching the panic means a window was built around it.
pub(crate) fn real_fft(data: &mut [f32]) -> &mut [Complex32] {
    macro_rules! rfft {
        ($f:ident) => {
            $f(data.try_into().expect("BUG: length checked by match")).as_mut_slice()
        };
    }
    match data.len() {
        4 => rfft!(rfft_4),
        8 => rfft!(rfft_8),
        16 => rfft!(rfft_16),
        32 => rfft!(rfft_32),
        64 => rfft!(rfft_64),
        128 => rfft!(rfft_128),
        256 => rfft!(rfft_256),
        512 => rfft!(rfft_512),
        1024 => rfft!(rfft_1024),
        2048 => rfft!(rfft_2048),
        4096 => rfft!(rfft_4096),
        8192 => rfft!(rfft_8192),
        16384 => rfft!(rfft_16384),
        32768 => rfft!(rfft_32768),
        n => panic!("invalid FFT length {n}"),
    }
}

/// Inverse complex FFT, in place, **unnormalized** (the output is `len` times
/// the input signal; the caller divides).
///
/// `data.len()` must be a power of two in `4..=32768`.
///
/// # Panics
///
/// On any other length, for the reason [`real_fft`] gives.
pub(crate) fn inverse_fft(data: &mut [Complex32]) {
    macro_rules! ifft {
        ($f:ident) => {{
            // The returned reference is `data` itself; the transform is in
            // place, so there is nothing to keep.
            let _ = $f(data.try_into().expect("BUG: length checked by match"));
        }};
    }
    match data.len() {
        4 => ifft!(ifft_4),
        8 => ifft!(ifft_8),
        16 => ifft!(ifft_16),
        32 => ifft!(ifft_32),
        64 => ifft!(ifft_64),
        128 => ifft!(ifft_128),
        256 => ifft!(ifft_256),
        512 => ifft!(ifft_512),
        1024 => ifft!(ifft_1024),
        2048 => ifft!(ifft_2048),
        4096 => ifft!(ifft_4096),
        8192 => ifft!(ifft_8192),
        16384 => ifft!(ifft_16384),
        32768 => ifft!(ifft_32768),
        n => panic!("invalid FFT length {n}"),
    }
}
