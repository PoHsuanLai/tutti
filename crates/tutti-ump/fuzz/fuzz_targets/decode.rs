#![no_main]
//! Any 16 bytes are four words. `decode` must not panic on them, a refusal must
//! hand the words back, and what decodes must survive an encode and a second
//! decode unchanged.

use libfuzzer_sys::fuzz_target;
use tutti_ump::{Ump, Unknown};

fuzz_target!(|data: &[u8]| {
    let mut bytes = [0u8; 16];
    let n = data.len().min(16);
    bytes[..n].copy_from_slice(&data[..n]);
    let mut words = [0u32; 4];
    for (word, chunk) in words.iter_mut().zip(bytes.chunks_exact(4)) {
        *word = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
    }
    let ump = Ump(words);
    match ump.decode() {
        Ok(message) => {
            let canonical = Ump::from(message);
            assert_eq!(canonical.decode(), Ok(message));
            assert_eq!(Ump::from(canonical.decode().unwrap()), canonical);
        }
        Err(Unknown(back)) => assert_eq!(back, ump),
    }
});
