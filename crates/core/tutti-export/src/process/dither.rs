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

pub(crate) fn apply_dither(
    left: &mut [f32],
    right: &mut [f32],
    target_bits: u16,
    state: &mut DitherState,
) {
    let lsb = 1.0 / (1 << (target_bits - 1)) as f32;

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
}
