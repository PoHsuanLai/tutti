//! The authentication digests (M2-124 sections 6.9 and 6.10).

use tutti_midi_network::{Digest, Nonce, Username};

#[test]
fn shared_secret_digest_matches_the_specification_example() {
    let nonce = Nonce::new(*b"nUWrn*@#$hjfwnkL").unwrap();
    let expected = [
        0x67, 0x6E, 0xBE, 0x82, 0x58, 0x7C, 0xEC, 0xA8, 0xF8, 0x2F, 0xC3, 0x33, 0xD7, 0x87, 0x95,
        0x1E, 0xEC, 0x2B, 0x00, 0xAD, 0x31, 0x61, 0x3C, 0xC4, 0xDB, 0x18, 0xCF, 0x27, 0x37, 0x3A,
        0xFB, 0x82,
    ];
    assert_eq!(Digest::of_secret(&nonce, "5483").get(), expected);
}

#[test]
fn user_digest_matches_the_specification_example() {
    let nonce = Nonce::new(*b"XI|~=NNRVaD;XCPL").unwrap();
    let expected = [
        0x3A, 0x2D, 0x5E, 0xBE, 0xEF, 0x92, 0xE8, 0x46, 0x35, 0x35, 0xC2, 0x7F, 0xB4, 0xB7, 0xB3,
        0xE2, 0xD1, 0xAC, 0xF5, 0x7A, 0x84, 0x4C, 0x6D, 0x08, 0x08, 0xE0, 0x41, 0xC1, 0x02, 0xB7,
        0xFF, 0x1A,
    ];
    let digest = Digest::of_user(&nonce, &Username::new("Rosa").unwrap(), "RPBqBno");
    assert_eq!(digest.get(), expected);
}

#[test]
fn digests_that_differ_in_one_byte_do_not_match() {
    let a = Digest::new([1; 32]);
    let mut other = [1; 32];
    other[31] = 2;
    assert!(a.matches(&a));
    assert!(!a.matches(&Digest::new(other)));
}

#[test]
fn a_nonce_must_be_ascii_and_entropy_makes_printable_ascii() {
    let mut bad = *b"0123456789abcdef";
    bad[3] = 0x80;
    assert!(Nonce::new(bad).is_none());
    let nonce = Nonce::from_entropy([0xFF; 16]);
    assert!(nonce.get().iter().all(|b| (32..=126).contains(b)));
    assert_ne!(Nonce::random(), Nonce::random(), "two random nonces differ");
}
