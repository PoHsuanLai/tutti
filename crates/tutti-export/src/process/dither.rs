use crate::options::{Dither, NoiseShapeOrder};

pub(crate) struct DitherState {
    random_state: u32,
    dither: Dither,
    shaper_l: NoiseShaper,
    shaper_r: NoiseShaper,
}

impl DitherState {
    pub fn new(dither: Dither) -> Self {
        let order = match dither {
            Dither::NoiseShaped(NoiseShapeOrder::Ninth) => NoiseShapeOrder::Ninth,
            _ => NoiseShapeOrder::Third,
        };
        Self {
            random_state: 0x12345678,
            dither,
            shaper_l: NoiseShaper::new(order),
            shaper_r: NoiseShaper::new(order),
        }
    }

    #[inline]
    fn random(&mut self) -> u32 {
        let mut x = self.random_state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.random_state = x;
        x
    }

    #[inline]
    fn rectangular_noise(&mut self) -> f32 {
        (self.random() as f32 / u32::MAX as f32) - 0.5
    }

    #[inline]
    fn triangular_noise(&mut self) -> f32 {
        let r1 = self.random() as f32 / u32::MAX as f32;
        let r2 = self.random() as f32 / u32::MAX as f32;
        r1 - r2
    }
}

pub(crate) fn apply_dither(
    left: &mut [f32],
    right: &mut [f32],
    target_bits: u16,
    state: &mut DitherState,
) {
    if matches!(state.dither, Dither::Off) {
        return;
    }

    let max_value = (1 << (target_bits - 1)) as f32;
    let lsb = 1.0 / max_value;

    match state.dither {
        Dither::Off => {}
        Dither::Rectangular => {
            for (l, r) in left.iter_mut().zip(right.iter_mut()) {
                *l += state.rectangular_noise() * lsb;
                *r += state.rectangular_noise() * lsb;
            }
        }
        Dither::Triangular => {
            for (l, r) in left.iter_mut().zip(right.iter_mut()) {
                *l += state.triangular_noise() * lsb;
                *r += state.triangular_noise() * lsb;
            }
        }
        Dither::NoiseShaped(_) => {
            for (l, r) in left.iter_mut().zip(right.iter_mut()) {
                let dither_l = state.triangular_noise() * lsb;
                let dither_r = state.triangular_noise() * lsb;

                *l = state.shaper_l.process(*l, dither_l, max_value);
                *r = state.shaper_r.process(*r, dither_r, max_value);
            }
        }
    }
}

/// FIR error-feedback noise shaper. The filter operates on quantization error:
/// each sample is adjusted by a weighted sum of recent errors, pushing noise
/// energy into ultrasonic frequencies where it is less audible.
struct NoiseShaper {
    coefficients: &'static [f32],
    errors: [f32; 9],
    pos: usize,
}

// 3rd-order: NTF(z) = (1 - 0.7*z^-1)^3, all zeros at z=0.7 (stable).
// ~12 dB low-frequency suppression at 44.1 kHz, HF boost ~6 dB.
// C(z) = 1 - NTF(z) = 2.1*z^-1 - 1.47*z^-2 + 0.343*z^-3
const THIRD_ORDER_COEFFS: &[f32] = &[2.1, -1.47, 0.343];

// 5th-order (labeled "Ninth" historically). NTF(z) = (1 - 0.68*z^-1)^5.
// ~20 dB low-frequency suppression, HF boost <8 dB. All NTF zeros at z=0.68.
const NINTH_ORDER_COEFFS: &[f32] = &[3.4, -4.6240, 3.1443, -1.0691, 0.1454];

impl NoiseShaper {
    fn new(order: NoiseShapeOrder) -> Self {
        let coefficients = match order {
            NoiseShapeOrder::Third => THIRD_ORDER_COEFFS,
            NoiseShapeOrder::Ninth => NINTH_ORDER_COEFFS,
        };
        Self {
            coefficients,
            errors: [0.0; 9],
            pos: 0,
        }
    }

    /// Feed one sample through the shaper. Returns the quantized output.
    /// The error feedback shapes both dither and quantization noise.
    #[inline]
    fn process(&mut self, input: f32, dither: f32, max_value: f32) -> f32 {
        let mut feedback = 0.0f32;
        let order = self.coefficients.len();
        for k in 0..order {
            let idx = (self.pos + 9 - 1 - k) % 9;
            feedback += self.coefficients[k] * self.errors[idx];
        }

        let shaped_input = input - feedback;
        let quantized = ((shaped_input + dither) * max_value).round() / max_value;

        self.errors[self.pos] = quantized - shaped_input;
        self.pos = (self.pos + 1) % 9;

        quantized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dither_state_creation() {
        let state = DitherState::new(Dither::Triangular);
        assert!(matches!(state.dither, Dither::Triangular));
    }

    #[test]
    fn test_no_dither() {
        let mut left = vec![0.5, -0.5, 0.25];
        let mut right = vec![0.5, -0.5, 0.25];
        let original_left = left.clone();
        let original_right = right.clone();

        let mut state = DitherState::new(Dither::Off);
        apply_dither(&mut left, &mut right, 16, &mut state);

        assert_eq!(left, original_left);
        assert_eq!(right, original_right);
    }

    #[test]
    fn test_rectangular_dither() {
        let mut left = vec![0.0; 1000];
        let mut right = vec![0.0; 1000];

        let mut state = DitherState::new(Dither::Rectangular);
        apply_dither(&mut left, &mut right, 16, &mut state);

        let non_zero = left.iter().filter(|&&x| x != 0.0).count();
        assert!(non_zero > 900, "Expected most samples to have dither noise");

        let max_noise = 1.0 / 32768.0; // 16-bit LSB
        for &sample in &left {
            assert!(
                sample.abs() < max_noise * 2.0,
                "Noise exceeds expected bounds"
            );
        }
    }

    #[test]
    fn test_triangular_dither() {
        let mut left = vec![0.0; 1000];
        let mut right = vec![0.0; 1000];

        let mut state = DitherState::new(Dither::Triangular);
        apply_dither(&mut left, &mut right, 16, &mut state);

        let max_noise = 1.0 / 32768.0;
        let max_sample = left.iter().map(|x| x.abs()).fold(0.0f32, f32::max);
        assert!(max_sample < max_noise * 3.0);
    }

    #[test]
    fn test_noise_shaped_third_order() {
        let mut left = vec![0.0; 4000];
        let mut right = vec![0.0; 4000];

        let mut state = DitherState::new(Dither::NoiseShaped(NoiseShapeOrder::Third));
        apply_dither(&mut left, &mut right, 16, &mut state);

        let max_sample = left.iter().map(|x| x.abs()).fold(0.0f32, f32::max);
        let lsb = 1.0 / 32768.0;
        assert!(
            max_sample < lsb * 10.0,
            "3rd-order shaper output too large: {max_sample}"
        );
    }

    #[test]
    fn test_noise_shaped_ninth_order() {
        let mut left = vec![0.0; 4000];
        let mut right = vec![0.0; 4000];

        let mut state = DitherState::new(Dither::NoiseShaped(NoiseShapeOrder::Ninth));
        apply_dither(&mut left, &mut right, 16, &mut state);

        let max_sample = left.iter().map(|x| x.abs()).fold(0.0f32, f32::max);
        let lsb = 1.0 / 32768.0;
        assert!(
            max_sample < lsb * 15.0,
            "9th-order shaper output too large: {max_sample}"
        );
    }

    #[test]
    fn test_noise_shaped_spectral_tilt() {
        let n = 8192;
        let mut flat_l = vec![0.0f32; n];
        let mut flat_r = vec![0.0f32; n];
        let mut shaped_l = vec![0.0f32; n];
        let mut shaped_r = vec![0.0f32; n];

        let mut flat_state = DitherState::new(Dither::Triangular);
        apply_dither(&mut flat_l, &mut flat_r, 16, &mut flat_state);

        let mut shaped_state = DitherState::new(Dither::NoiseShaped(NoiseShapeOrder::Third));
        apply_dither(&mut shaped_l, &mut shaped_r, 16, &mut shaped_state);

        let low_bins = n / 4;
        let flat_low_energy: f32 = low_band_energy(&flat_l, low_bins);
        let shaped_low_energy: f32 = low_band_energy(&shaped_l, low_bins);

        assert!(
            shaped_low_energy < flat_low_energy,
            "Shaped low energy ({shaped_low_energy}) should be less than flat ({flat_low_energy})"
        );
    }

    fn low_band_energy(signal: &[f32], bins: usize) -> f32 {
        let n = signal.len();
        let mut energy = 0.0f32;
        for k in 1..bins {
            let mut re = 0.0f32;
            let mut im = 0.0f32;
            let freq = 2.0 * std::f32::consts::PI * k as f32 / n as f32;
            for (i, &s) in signal.iter().enumerate() {
                let angle = freq * i as f32;
                re += s * angle.cos();
                im += s * angle.sin();
            }
            energy += re * re + im * im;
        }
        energy
    }
}
