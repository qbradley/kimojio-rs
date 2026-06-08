// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::fmt;

/// Authentication mode selected by the caller.
#[derive(Clone, Eq, PartialEq)]
pub enum AuthMode {
    Identity { client_id: Option<String> },
    Bearer { token: String },
    Key { account: String, secret: KeySecret },
    SignedSource(SignedSource),
    Emulator { account: String, secure: bool },
}

impl fmt::Debug for AuthMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Identity { client_id } => f
                .debug_struct("Identity")
                .field("client_id", client_id)
                .finish(),
            Self::Bearer { .. } => f
                .debug_struct("Bearer")
                .field("token", &"<redacted>")
                .finish(),
            Self::Key { account, .. } => f
                .debug_struct("Key")
                .field("account", account)
                .field("secret", &"<redacted>")
                .finish(),
            Self::SignedSource(source) => f.debug_tuple("SignedSource").field(source).finish(),
            Self::Emulator { account, secure } => f
                .debug_struct("Emulator")
                .field("account", account)
                .field("secure", secure)
                .finish(),
        }
    }
}

impl AuthMode {
    /// Returns a stable mode name for diagnostics and tests.
    pub fn mode_name(&self) -> &'static str {
        match self {
            Self::Identity { .. } => "identity",
            Self::Bearer { .. } => "bearer",
            Self::Key { .. } => "key",
            Self::SignedSource(_) => "signed-source",
            Self::Emulator { .. } => "emulator",
        }
    }
}

/// Redacted key secret material.
#[derive(Clone, Eq, PartialEq)]
pub struct KeySecret {
    bytes: Vec<u8>,
}

impl fmt::Debug for KeySecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("KeySecret(<redacted>)")
    }
}

impl KeySecret {
    /// Creates key secret material.
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            bytes: bytes.into(),
        }
    }

    /// Returns the raw secret bytes for signing code.
    pub fn expose(&self) -> &[u8] {
        &self.bytes
    }
}

/// Signed source reference used for copy-style operations.
#[derive(Clone, Eq, PartialEq)]
pub struct SignedSource {
    reference: String,
    authorization: Option<String>,
}

impl fmt::Debug for SignedSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignedSource")
            .field("reference", &"<redacted>")
            .field(
                "authorization",
                &self.authorization.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl SignedSource {
    /// Creates a signed source reference.
    pub fn new(reference: impl Into<String>) -> Self {
        Self {
            reference: reference.into(),
            authorization: None,
        }
    }

    /// Adds source authorization material.
    pub fn with_authorization(mut self, authorization: impl Into<String>) -> Self {
        self.authorization = Some(authorization.into());
        self
    }

    /// Returns the source reference.
    pub fn reference(&self) -> &str {
        &self.reference
    }

    /// Returns optional authorization.
    pub fn authorization(&self) -> Option<&str> {
        self.authorization.as_deref()
    }
}

/// Per-attempt auth refresh decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthRefresh {
    Reuse,
    Refreshed(AuthMode),
}

/// Context passed to auth providers before each attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuthRefreshContext {
    pub attempt: u32,
    pub force_refresh: bool,
}

/// Caller-supplied authorization provider.
pub trait AuthProvider {
    /// Returns the auth mode to use for an attempt.
    fn auth_for_attempt(&mut self, context: AuthRefreshContext) -> AuthRefresh;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_modes_have_stable_names() {
        let modes = [
            AuthMode::Identity { client_id: None },
            AuthMode::Bearer { token: "t".into() },
            AuthMode::Key {
                account: "a".into(),
                secret: KeySecret::new(b"k".to_vec()),
            },
            AuthMode::SignedSource(SignedSource::new("src")),
            AuthMode::Emulator {
                account: "dev".into(),
                secure: true,
            },
        ];

        assert_eq!(
            modes.map(|mode| mode.mode_name()),
            ["identity", "bearer", "key", "signed-source", "emulator"]
        );
    }

    #[test]
    fn auth_debug_redacts_secrets() {
        let bearer = format!(
            "{:?}",
            AuthMode::Bearer {
                token: "secret-token".into()
            }
        );
        let key = format!("{:?}", KeySecret::new(b"secret-key".to_vec()));
        let source = format!(
            "{:?}",
            SignedSource::new("https://source.example/blob?sig=secret-sas")
                .with_authorization("secret-auth")
        );

        assert!(!bearer.contains("secret-token"));
        assert!(!key.contains("secret-key"));
        assert!(!source.contains("secret-sas"));
        assert!(!source.contains("secret-auth"));
    }

    #[test]
    fn signed_source_preserves_optional_authorization() {
        let source = SignedSource::new("https://source").with_authorization("auth");

        assert_eq!(source.reference(), "https://source");
        assert_eq!(source.authorization(), Some("auth"));
    }

    struct CountingProvider {
        refreshes: u32,
    }

    impl AuthProvider for CountingProvider {
        fn auth_for_attempt(&mut self, context: AuthRefreshContext) -> AuthRefresh {
            if context.force_refresh {
                self.refreshes += 1;
                AuthRefresh::Refreshed(AuthMode::Bearer {
                    token: format!("token-{}", context.attempt),
                })
            } else {
                AuthRefresh::Reuse
            }
        }
    }

    #[test]
    fn auth_provider_can_refresh_per_attempt() {
        let mut provider = CountingProvider { refreshes: 0 };

        assert_eq!(
            provider.auth_for_attempt(AuthRefreshContext {
                attempt: 1,
                force_refresh: false
            }),
            AuthRefresh::Reuse
        );
        assert_eq!(
            provider.auth_for_attempt(AuthRefreshContext {
                attempt: 2,
                force_refresh: true
            }),
            AuthRefresh::Refreshed(AuthMode::Bearer {
                token: "token-2".into()
            })
        );
        assert_eq!(provider.refreshes, 1);
    }
}
