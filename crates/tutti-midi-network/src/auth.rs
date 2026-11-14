//! The authentication values of an Invitation (M2-124 sections 6.7 to 6.10):
//! the [`Nonce`] a Host issues and the [`Digest`] a Client answers with.

use sha2::{Digest as _, Sha256};

use crate::Username;

/// The 16 ASCII bytes a Host issues to a Client that must authenticate.
///
/// A Client hashes it with its secret, so a digest from an earlier session
/// does not authenticate a new one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Nonce([u8; 16]);

impl Nonce {
    /// The nonce with these bytes, or `None` when one is not ASCII.
    #[must_use]
    pub fn new(bytes: [u8; 16]) -> Option<Self> {
        bytes.is_ascii().then_some(Self(bytes))
    }

    /// A nonce of printable ASCII (`32..=126`) from arbitrary bytes: each byte
    /// picks a character.
    #[must_use]
    pub fn from_entropy(entropy: [u8; 16]) -> Self {
        Self(entropy.map(|b| 32 + b % 95))
    }

    /// A nonce from the operating system's random bytes.
    ///
    /// # Panics
    ///
    /// When the operating system has no random bytes to give.
    #[must_use]
    pub fn random() -> Self {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).expect("the operating system provides random bytes");
        Self::from_entropy(bytes)
    }

    /// The 16 bytes.
    #[must_use]
    pub const fn get(&self) -> [u8; 16] {
        self.0
    }
}

/// A SHA-256 authentication digest (M2-124 sections 6.9 and 6.10).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Digest([u8; 32]);

impl Digest {
    /// A digest from its 32 bytes.
    #[must_use]
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The 32 bytes.
    #[must_use]
    pub const fn get(&self) -> [u8; 32] {
        self.0
    }

    /// The digest of the nonce followed by a shared secret.
    #[must_use]
    pub fn of_secret(nonce: &Nonce, secret: &str) -> Self {
        Self::hash(&[&nonce.0, secret.as_bytes()])
    }

    /// The digest of the nonce, the user name and the password, in that order.
    #[must_use]
    pub fn of_user(nonce: &Nonce, username: &Username, password: &str) -> Self {
        Self::hash(&[&nonce.0, username.as_str().as_bytes(), password.as_bytes()])
    }

    /// Whether the digests are equal, comparing every byte whatever the
    /// first difference, so the time taken does not say how much matched.
    #[must_use]
    pub fn matches(&self, other: &Self) -> bool {
        self.0
            .iter()
            .zip(&other.0)
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0
    }

    fn hash(parts: &[&[u8]]) -> Self {
        let mut hasher = Sha256::new();
        for part in parts {
            hasher.update(part);
        }
        Self(hasher.finalize().into())
    }
}
