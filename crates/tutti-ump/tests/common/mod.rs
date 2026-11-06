//! Shared by the property and oracle tests: shaping random words into the
//! valid range of one message family, so a random search reaches the deep
//! messages (a chord name needs six valid nibbles). The shaping only biases the
//! search; no expected value comes from it.
#![allow(dead_code)]

/// `w` pushed into the valid range of the family `sel` selects.
pub fn shape(mut w: [u32; 4], sel: u8) -> [u32; 4] {
    let g = (w[0] >> 24) & 0xf;
    let ch = (w[0] >> 16) & 0xf;
    match sel % 12 {
        0 => w[0] = ((w[0] >> 20) % 5) << 20 | (w[0] & 0xffff), // utility
        1 => {
            const S: [u32; 10] = [0xF1, 0xF2, 0xF3, 0xF6, 0xF8, 0xFA, 0xFB, 0xFC, 0xFE, 0xFF];
            w[0] = 0x1000_0000 | g << 24 | S[(w[0] >> 20) as usize % 10] << 16 | (w[0] & 0xffff);
        }
        2 => {
            w[0] = 0x2000_0000 | g << 24 | (8 + (w[0] >> 20) % 7) << 20 | ch << 16 | (w[0] & 0xffff)
        }
        3 => {
            w[0] = 0x3000_0000
                | g << 24
                | ((w[0] >> 20) % 4) << 20
                | ((w[0] >> 16) % 7) << 16
                | (w[0] & 0xffff)
        }
        4 | 5 => {
            let op =
                [0, 1, 2, 3, 4, 5, 6, 8, 9, 10, 11, 12, 13, 14, 15][(w[0] >> 20) as usize % 15];
            w[0] = 0x4000_0000 | g << 24 | op << 20 | ch << 16 | (w[0] & 0xffff);
            if op == 8 || op == 9 {
                w[0] = (w[0] & !0xff) | (w[0] & 3);
            }
        }
        6 => {
            let low = if (w[0] >> 20) % 4 == 3 && w[0] & 1 == 1 {
                0xF
            } else {
                1 + (w[0] >> 16) % 14
            };
            w[0] = 0x5000_0000 | g << 24 | ((w[0] >> 20) % 4) << 20 | low << 16 | (w[0] & 0xffff);
        }
        7 => w[0] = 0x5000_0000 | g << 24 | (8 + (w[0] >> 20) % 2) << 20 | (w[0] & 0xf_ffff),
        8 => {
            // Flex setup and text.
            let (bank, status) = match (w[0] >> 20) % 4 {
                0 => (0, 0),
                1 => (0, 1),
                2 => (0, 2),
                _ => (1 + (w[0] >> 8) % 2, (w[0] >> 4) % 5),
            };
            w[0] = 0xD000_0000
                | g << 24
                | (w[0] & (0xC0_0000 * u32::from(bank != 0)))
                | 1 << 20
                | bank << 8
                | status;
        }
        9 => {
            // Key signature and chord name with valid nibbles.
            const ACC: [u32; 5] = [0, 1, 2, 0xF, 0xE];
            let n = |x: u32| ACC[(x >> 3) as usize % 5];
            let alt = |x: u32| ((x >> 8) % 5) << 4 | (x & 0xf);
            if sel & 0x10 == 0 {
                w[0] = 0xD010_0005 | g << 24;
                w[1] = (w[1] & 0xFF00_0000 & 0xF7FF_FFFF) | (w[1] & 7) << 24;
            } else {
                w[0] = 0xD010_0006 | g << 24;
                w[1] = n(w[1]) << 28
                    | (w[2] % 8) << 24
                    | (w[3] % 0x1C) << 16
                    | alt(w[1]) << 8
                    | alt(w[1] >> 12);
                w[2] = alt(w[2]) << 24 | alt(w[2] >> 12) << 16;
                w[3] = if w[3] & 0x100 == 0 {
                    0x8000_0000 | (w[3] % 0x1C) << 16
                } else {
                    n(w[3] >> 20) << 28 | (1 + w[3] % 7) << 24 | (w[3] % 0x1C) << 16
                } | alt(w[3] >> 4) << 8
                    | alt(w[3] >> 16);
            }
        }
        10 => {
            const S: [u32; 10] = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x10, 0x20, 0x21];
            let status = S[(w[0] >> 20) as usize % 10];
            w[0] = 0xF000_0000
                | (u32::from(matches!(status, 3 | 4)) * (w[0] & 0x0C00_0000))
                | status << 16
                | (w[0] & 0xffff);
            match status {
                1 => w[1] = (w[1] & 0x80FF_FFFF) | ((w[1] >> 24) % 33) << 24,
                5 | 6 => w[0] = (w[0] & !0xff00) | (1 + (w[0] >> 8) % 2) << 8,
                0x10 => {
                    w[0] = (w[0] & !0xff00)
                        | if w[0] & 0x100 == 0 {
                            0xFF
                        } else {
                            (w[0] >> 9) % 32
                        } << 8
                }
                0x11 => {
                    w[0] = (w[0] & !0x7F3F)
                        | ((w[0] >> 8) % 32) << 8
                        | (1 + w[0] % 3)
                        | ((w[0] >> 2) % 3) << 2;
                    w[1] =
                        ((w[1] >> 24) % 16) << 24 | (1 + (w[1] >> 16) % 16) << 16 | (w[1] & 0xffff);
                }
                0x12 => w[0] = (w[0] & !0xff00) | ((w[0] >> 8) % 32) << 8,
                _ => {}
            }
        }
        _ => {}
    }
    w
}
