// Derived from rustysynth 1.3.6 (MIT), Copyright (C) 2022 Nobuaki Tanaka, after
// MeltySynth (Copyright (C) 2021 Nobuaki Tanaka). The MIT notice is
// LICENSE-RUSTYSYNTH.txt, at the root of this crate.

#[non_exhaustive]
pub(crate) struct ArrayMath {}

impl ArrayMath {
    pub(crate) fn multiply_add(a: f32, x: &[f32], destination: &mut [f32]) {
        for (x, destination) in x.iter().zip(destination.iter_mut()) {
            *destination += a * *x;
        }
    }

    pub(crate) fn multiply_add_slope(a: f32, step: f32, x: &[f32], destination: &mut [f32]) {
        let mut a = a;
        for (x, destination) in x.iter().zip(destination.iter_mut()) {
            *destination += a * *x;
            a += step;
        }
    }
}
