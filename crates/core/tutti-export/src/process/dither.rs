use crate::options::Dither;

pub(crate) struct DitherState {
    random_state: u32,
    dither: Dither,
}

impl DitherState {
    pub fn new(dither: Dither) -> Self {
        Self {
            random_state: 0x12345678,
            dither,
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

/// Dither one channel plane in place. Dither is independent per channel — its
/// only cross-block state is the shared RNG on `state` — so a multichannel
/// signal dithers by calling this once per plane in a fixed channel order, which
/// keeps every channel's noise drawn from the same continuous sequence.
pub(crate) fn apply_dither(plane: &mut [f32], target_bits: u16, state: &mut DitherState) {
    let lsb = 1.0 / (1 << (target_bits - 1)) as f32;

    match state.dither {
        Dither::Off => {}
        Dither::Rectangular => {
            for s in plane.iter_mut() {
                *s += state.rectangular_noise() * lsb;
            }
        }
        Dither::Triangular => {
            for s in plane.iter_mut() {
                *s += state.triangular_noise() * lsb;
            }
        }
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
        let original_left = left.clone();

        let mut state = DitherState::new(Dither::Off);
        apply_dither(&mut left, 16, &mut state);

        assert_eq!(left, original_left);
    }

    #[test]
    fn test_rectangular_dither() {
        let mut left = vec![0.0; 1000];

        let mut state = DitherState::new(Dither::Rectangular);
        apply_dither(&mut left, 16, &mut state);

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

        let mut state = DitherState::new(Dither::Triangular);
        apply_dither(&mut left, 16, &mut state);

        let max_noise = 1.0 / 32768.0;
        let max_sample = left.iter().map(|x| x.abs()).fold(0.0f32, f32::max);
        assert!(max_sample < max_noise * 3.0);
    }
}
