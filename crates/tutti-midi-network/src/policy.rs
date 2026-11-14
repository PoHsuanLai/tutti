//! What a Client can prove ([`Credentials`]) and what a Host demands
//! ([`Admission`]).

use std::fmt;

use crate::{AuthMethods, Username};

/// The secrets a Client answers a challenge with.
///
/// A Client that holds none cannot answer one, and advertises no
/// authentication method in its Invitation.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Credentials {
    pub(crate) secret: Option<String>,
    pub(crate) user: Option<(Username, String)>,
}

impl Credentials {
    /// No secrets.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Adds a shared secret, such as a 4-digit code.
    #[must_use]
    pub fn with_secret(mut self, secret: impl Into<String>) -> Self {
        self.secret = Some(secret.into());
        self
    }

    /// Adds a user name and its password.
    #[must_use]
    pub fn with_user(mut self, username: Username, password: impl Into<String>) -> Self {
        self.user = Some((username, password.into()));
        self
    }

    /// The methods these credentials can answer.
    #[must_use]
    pub fn methods(&self) -> AuthMethods {
        let secret = if self.secret.is_some() {
            AuthMethods::SHARED_SECRET
        } else {
            AuthMethods::NONE
        };
        let user = if self.user.is_some() {
            AuthMethods::USER
        } else {
            AuthMethods::NONE
        };
        secret.union(user)
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("methods", &self.methods())
            .finish_non_exhaustive()
    }
}

/// How a Host decides whether an Invitation becomes a Session.
#[derive(Clone, PartialEq, Eq)]
pub enum Admission {
    /// Every Invitation is accepted.
    Open,
    /// The application decides for each Invitation: the Host answers Pending
    /// and reports an [`Event::Confirm`](crate::Event::Confirm).
    Confirm,
    /// A Client must answer a challenge with this shared secret.
    SharedSecret(String),
    /// A Client must answer a challenge with the password of one of these
    /// users.
    Users(Vec<(Username, String)>),
}

impl Admission {
    /// The method a Client needs to be admitted, if one is.
    pub(crate) fn required(&self) -> AuthMethods {
        match self {
            Self::Open | Self::Confirm => AuthMethods::NONE,
            Self::SharedSecret(_) => AuthMethods::SHARED_SECRET,
            Self::Users(_) => AuthMethods::USER,
        }
    }
}

impl fmt::Debug for Admission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open => f.write_str("Open"),
            Self::Confirm => f.write_str("Confirm"),
            Self::SharedSecret(_) => f.write_str("SharedSecret(..)"),
            Self::Users(users) => write!(f, "Users({} users)", users.len()),
        }
    }
}
