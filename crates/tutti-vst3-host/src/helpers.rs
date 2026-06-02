//! Small string conversion helpers for VST3 C buffers.

pub fn utf16_to_string(bytes: &[u16]) -> String {
    let end = bytes.iter().position(|&c| c == 0).unwrap_or(bytes.len());
    String::from_utf16_lossy(&bytes[..end])
}

pub fn c_str_to_string(bytes: &[i8]) -> String {
    let bytes: Vec<u8> = bytes
        .iter()
        .take_while(|&&b| b != 0)
        .map(|&b| b as u8)
        .collect();
    String::from_utf8_lossy(&bytes).to_string()
}
