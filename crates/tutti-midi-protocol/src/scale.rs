//! Min-Center-Max upscaling (M2-115 section 3).

/// Widens `value` of `src` bits to `dst` bits. `1 < src < dst <= 32`.
///
/// Values up to the centre shift left, which keeps the steps even towards the
/// centre; values above it also repeat their low `src - 1` bits into the new
/// low bits, which lands the maximum on the maximum.
#[inline]
pub(crate) const fn upscale(value: u32, src: u32, dst: u32) -> u32 {
    let scale_bits = dst - src;
    let shifted = value << scale_bits;
    if value <= 1 << (src - 1) {
        return shifted;
    }
    let repeat_bits = src - 1;
    let mut repeat = value & ((1 << repeat_bits) - 1);
    repeat = if scale_bits > repeat_bits {
        repeat << (scale_bits - repeat_bits)
    } else {
        repeat >> (repeat_bits - scale_bits)
    };
    let mut out = shifted;
    while repeat != 0 {
        out |= repeat;
        repeat >>= repeat_bits;
    }
    out
}

/// A 7-bit velocity as the 16-bit field of a Note On or Note Off.
#[inline]
pub(crate) const fn velocity(v: u8) -> u16 {
    upscale(v as u32, 7, 16) as u16
}

/// A 7-bit controller or pressure value as 32 bits.
#[inline]
pub(crate) const fn value(v: u8) -> u32 {
    upscale(v as u32, 7, 32)
}

/// A 14-bit pitch bend as 32 bits.
#[inline]
pub(crate) const fn bend(v: u16) -> u32 {
    upscale(v as u32, 14, 32)
}

/// Whether the Registered Controller `bank`/`index` is zero-extended (M2-115
/// section 4.1) rather than scaled Min-Center-Max: the existing RPNs, bank 0
/// and index 0 to 31, whose data is a fixed-point number or a pair of fields.
/// The translator widens by it and the narrower narrows by it, so the two
/// cannot disagree.
#[inline]
pub(crate) const fn zero_extended(bank: tutti_ump::U7, index: tutti_ump::U7) -> bool {
    bank.get() == 0 && index.get() < 32
}
