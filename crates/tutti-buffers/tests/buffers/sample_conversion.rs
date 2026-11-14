//! Every `Sample` to `Sample` conversion at its edge values.

use tutti_buffers::{I24, Sample};

/// An integer format, by its raw `i32` value, so one body covers all five.
trait Int: Sample + Copy {
    const BITS: u32;
    fn make(raw: i32) -> Self;
    fn raw(self) -> i32;
}

macro_rules! int {
    ($t:ty, $bits:literal) => {
        impl Int for $t {
            const BITS: u32 = $bits;
            fn make(raw: i32) -> Self {
                Self::try_from(raw).unwrap()
            }
            fn raw(self) -> i32 {
                i32::from(self)
            }
        }
    };
}
int!(i8, 8);
int!(i16, 16);
int!(i32, 32);

impl Int for I24 {
    const BITS: u32 = 24;
    fn make(raw: i32) -> Self {
        I24::of(raw)
    }
    fn raw(self) -> i32 {
        self.get()
    }
}

fn min<T: Int>() -> T {
    T::make((-(1i64 << (T::BITS - 1))) as i32)
}
fn max<T: Int>() -> T {
    T::make(((1i64 << (T::BITS - 1)) - 1) as i32)
}
fn full_scale<T: Int>() -> f64 {
    (1u64 << (T::BITS - 1)) as f64
}

/// Integer to float is `x / 2^(n-1)`.
fn int_to_float_edges<T: Int>() {
    let s = full_scale::<T>();
    assert_eq!(min::<T>().convert_to::<f64>(), -1.0);
    assert_eq!(T::make(0).convert_to::<f64>(), 0.0);
    assert_eq!(max::<T>().convert_to::<f64>(), (s - 1.0) / s);
    assert_eq!(T::make(1).convert_to::<f64>(), 1.0 / s);
    assert_eq!(T::make(-1).convert_to::<f64>(), -1.0 / s);
    // f32 rounds the exact quotient to nearest even, so MAX of a wide format
    // reaches 1.0.
    assert_eq!(min::<T>().convert_to::<f32>(), -1.0);
    assert_eq!(T::make(0).convert_to::<f32>(), 0.0);
    assert_eq!(max::<T>().convert_to::<f32>(), ((s - 1.0) / s) as f32);
}

/// Float to integer is `x * 2^(n-1)`, half to even, clipped; NaN is zero.
fn float_to_int_edges<T: Int, F: Sample + Copy>(f: impl Fn(f64) -> F) {
    let s = (1i64 << (T::BITS - 1)) as f64;
    let to = |x: f64| f(x).convert_to::<T>().raw();
    let (lo, hi) = (min::<T>().raw(), max::<T>().raw());
    assert_eq!(to(0.0), 0);
    assert_eq!(to(-0.0), 0);
    assert_eq!(to(1.0), hi, "+1.0 clips to MAX");
    assert_eq!(to(-1.0), lo, "-1.0 is MIN");
    assert_eq!(to(0.5), 1 << (T::BITS - 2));
    assert_eq!(to(-0.5), -(1 << (T::BITS - 2)));
    assert_eq!(to(1.5), hi);
    assert_eq!(to(-1.5), lo);
    assert_eq!(to(1.0e30), hi);
    assert_eq!(to(-1.0e30), lo);
    assert_eq!(to(f64::INFINITY), hi);
    assert_eq!(to(f64::NEG_INFINITY), lo);
    assert_eq!(to(f64::NAN), 0, "NaN is silence");
    // Ties go to the even integer: k + 0.5 at full scale.
    let tie = |k: f64| (k + 0.5) / s;
    assert_eq!(to(tie(0.0)), 0);
    assert_eq!(to(tie(1.0)), 2);
    assert_eq!(to(tie(2.0)), 2);
    assert_eq!(to(tie(3.0)), 4);
    assert_eq!(to(-tie(0.0)), 0);
    assert_eq!(to(-tie(1.0)), -2);
    assert_eq!(to(-tie(2.0)), -2);
    // Just off a tie rounds to the nearer integer.
    assert_eq!(to(tie(0.0) * 1.01), 1);
    assert_eq!(to(tie(1.0) * 0.99), 1);
}

/// Float to integer to float is within one least significant bit.
fn float_int_float_within_one_lsb<T: Int>() {
    let lsb = 1.0 / full_scale::<T>();
    for i in -1000..=1000 {
        let x = f64::from(i) / 1000.0;
        let back = x.convert_to::<T>().convert_to::<f64>();
        assert!(
            (back - x).abs() <= lsb,
            "{x} came back as {back}, more than one LSB ({lsb}) off"
        );
    }
}

/// Integer to float to the same integer is exact, through f64.
fn int_float_int_exact<T: Int>() {
    let hi = i64::from(max::<T>().raw());
    let lo = i64::from(min::<T>().raw());
    let probes = [lo, lo + 1, -257, -2, -1, 0, 1, 2, 257, hi - 1, hi];
    for r in probes.into_iter().filter(|r| (lo..=hi).contains(r)) {
        let t = T::make(r as i32);
        assert_eq!(
            t.convert_to::<f64>().convert_to::<T>().raw(),
            t.raw(),
            "{r}"
        );
    }
}

macro_rules! per_format {
    ($($name:ident: $t:ty),*) => {$(
        mod $name {
            use super::*;

            #[test]
            fn integer_to_float_divides_by_two_to_the_n_minus_one() {
                int_to_float_edges::<$t>();
            }

            #[test]
            fn f64_to_integer_scales_rounds_half_even_clips_and_maps_nan_to_zero() {
                float_to_int_edges::<$t, f64>(|x| x);
            }

            #[test]
            fn f32_to_integer_scales_rounds_half_even_clips_and_maps_nan_to_zero() {
                float_to_int_edges::<$t, f32>(|x| x as f32);
            }

            #[test]
            fn float_to_integer_to_float_is_within_one_lsb() {
                float_int_float_within_one_lsb::<$t>();
            }

            #[test]
            fn integer_to_f64_to_integer_is_exact() {
                int_float_int_exact::<$t>();
            }
        }
    )*};
}
per_format!(i8_fmt: i8, i16_fmt: i16, i24_fmt: I24, i32_fmt: i32);

#[test]
fn integer_to_f32_to_integer_is_exact_up_to_24_bits() {
    for v in i8::MIN..=i8::MAX {
        assert_eq!(v.convert_to::<f32>().convert_to::<i8>(), v);
    }
    for v in i16::MIN..=i16::MAX {
        assert_eq!(v.convert_to::<f32>().convert_to::<i16>(), v);
    }
    for r in [
        -8_388_608, -8_388_607, -12_345, -1, 0, 1, 99_999, 8_388_606, 8_388_607,
    ] {
        let v = I24::of(r);
        assert_eq!(v.convert_to::<f32>().convert_to::<I24>(), v);
    }
}

#[test]
fn i32_does_not_survive_f32_because_its_mantissa_is_24_bits() {
    let v = 2_147_483_007i32;
    assert_ne!(v.convert_to::<f32>().convert_to::<i32>(), v);
    assert_eq!(v.convert_to::<f64>().convert_to::<i32>(), v);
}

#[test]
fn float_to_float_is_a_plain_cast() {
    assert_eq!(0.1f64.convert_to::<f32>(), 0.1f32);
    assert_eq!(0.1f32.convert_to::<f64>(), f64::from(0.1f32));
    assert_eq!(1.0e300f64.convert_to::<f32>(), f32::INFINITY);
    assert_eq!(f64::NEG_INFINITY.convert_to::<f32>(), f32::NEG_INFINITY);
    assert!(f64::NAN.convert_to::<f32>().is_nan());
    assert!(f32::NAN.convert_to::<f64>().is_nan());
    assert_eq!(2.5f32.convert_to::<f64>(), 2.5);
    assert_eq!(-3.0f64.convert_to::<f64>(), -3.0);
    assert_eq!(
        2.0f32.convert_to::<f32>(),
        2.0,
        "no clipping between floats"
    );
}

#[test]
fn widening_an_integer_shifts_left_by_the_width_difference() {
    assert_eq!(0x12i8.convert_to::<i16>(), 0x1200);
    assert_eq!(i8::MIN.convert_to::<i16>(), i16::MIN);
    assert_eq!(i8::MAX.convert_to::<i16>(), 0x7f00);
    assert_eq!(0x1234i16.convert_to::<I24>(), I24::of(0x12_3400));
    assert_eq!(i16::MIN.convert_to::<I24>(), I24::MIN);
    assert_eq!(0x1234i16.convert_to::<i32>(), 0x1234_0000);
    assert_eq!(I24::of(0x12_3456).convert_to::<i32>(), 0x1234_5600);
    assert_eq!(I24::MIN.convert_to::<i32>(), i32::MIN);
    assert_eq!(I24::MAX.convert_to::<i32>(), 0x7fff_ff00);
    assert_eq!((-1i8).convert_to::<i32>(), -(1 << 24));
}

#[test]
fn widening_then_narrowing_returns_every_value() {
    for v in i8::MIN..=i8::MAX {
        assert_eq!(v.convert_to::<i16>().convert_to::<i8>(), v);
        assert_eq!(v.convert_to::<I24>().convert_to::<i8>(), v);
        assert_eq!(v.convert_to::<i32>().convert_to::<i8>(), v);
    }
    for v in i16::MIN..=i16::MAX {
        assert_eq!(v.convert_to::<I24>().convert_to::<i16>(), v);
        assert_eq!(v.convert_to::<i32>().convert_to::<i16>(), v);
    }
}

#[test]
fn narrowing_an_integer_rounds_half_to_even() {
    // i16 to i8 divides by 256.
    assert_eq!(127i16.convert_to::<i8>(), 0);
    assert_eq!(128i16.convert_to::<i8>(), 0, "0.5 ties to 0");
    assert_eq!(129i16.convert_to::<i8>(), 1);
    assert_eq!(384i16.convert_to::<i8>(), 2, "1.5 ties to 2");
    assert_eq!(640i16.convert_to::<i8>(), 2, "2.5 ties to 2");
    assert_eq!(896i16.convert_to::<i8>(), 4, "3.5 ties to 4");
    assert_eq!((-128i16).convert_to::<i8>(), 0, "-0.5 ties to 0");
    assert_eq!((-129i16).convert_to::<i8>(), -1);
    assert_eq!((-384i16).convert_to::<i8>(), -2);
    assert_eq!((-640i16).convert_to::<i8>(), -2);
    // i32 to i16 divides by 65536.
    assert_eq!(0x8000i32.convert_to::<i16>(), 0);
    assert_eq!(0x1_8000i32.convert_to::<i16>(), 2);
    assert_eq!(0x2_8000i32.convert_to::<i16>(), 2);
    assert_eq!((-0x1_8000i32).convert_to::<i16>(), -2);
    // I24 to i16 divides by 256.
    assert_eq!(I24::of(0x180).convert_to::<i16>(), 2);
    assert_eq!(I24::of(0x280).convert_to::<i16>(), 2);
    assert_eq!(I24::of(-0x180).convert_to::<i16>(), -2);
}

#[test]
fn narrowing_an_integer_clips_at_the_target_range() {
    assert_eq!(
        i16::MAX.convert_to::<i8>(),
        i8::MAX,
        "32767 / 256 rounds to 128, clipped"
    );
    assert_eq!(i16::MIN.convert_to::<i8>(), i8::MIN);
    assert_eq!(
        32_640i16.convert_to::<i8>(),
        127,
        "127.5 ties to 128, clipped"
    );
    assert_eq!(i32::MAX.convert_to::<i16>(), i16::MAX);
    assert_eq!(i32::MIN.convert_to::<i16>(), i16::MIN);
    assert_eq!(i32::MAX.convert_to::<I24>(), I24::MAX);
    assert_eq!(i32::MIN.convert_to::<I24>(), I24::MIN);
    assert_eq!(I24::MAX.convert_to::<i16>(), i16::MAX);
    assert_eq!(I24::MIN.convert_to::<i16>(), i16::MIN);
    assert_eq!(I24::MAX.convert_to::<i8>(), i8::MAX);
    assert_eq!(i32::MAX.convert_to::<i8>(), i8::MAX);
}

#[test]
fn integer_to_integer_equals_going_through_the_float_rule_for_every_i16() {
    // The exact-integer path and the spec's float composition agree.
    for v in i16::MIN..=i16::MAX {
        let via_float = v.convert_to::<f64>().convert_to::<i8>();
        assert_eq!(v.convert_to::<i8>(), via_float, "{v}");
    }
}

#[test]
fn integer_conversion_to_itself_is_the_identity() {
    for v in [i8::MIN, -1, 0, 1, i8::MAX] {
        assert_eq!(v.convert_to::<i8>(), v);
    }
    assert_eq!(i32::MIN.convert_to::<i32>(), i32::MIN);
    assert_eq!(I24::MAX.convert_to::<I24>(), I24::MAX);
}

#[test]
fn silence_converts_to_silence() {
    assert_eq!(
        <f32 as Sample>::SILENCE.convert_to::<i16>(),
        <i16 as Sample>::SILENCE
    );
    assert_eq!(<i8 as Sample>::SILENCE.convert_to::<f64>(), 0.0);
    assert_eq!(<I24 as Sample>::SILENCE, I24::ZERO);
}
