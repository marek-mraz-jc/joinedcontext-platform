//! A string whose `Debug` does not print it (CC-06, OPS-12, SEC-GAP-03).
//!
//! A configuration struct is the natural thing to `Debug`-print while chasing a startup
//! problem, and every such struct that holds a credential is one `tracing::debug!(?config)`
//! away from putting it in the cluster's logs, which `CLAUDE.md` forbids outright. Writing a
//! `Debug` by hand for each of them works until somebody adds a field, so the redaction lives
//! on the value instead: a [`Secret`] prints as `<redacted>` wherever it is nested, copied or
//! moved, and the plaintext comes out only by asking for it by name.

use std::fmt;

use zeroize::Zeroize;

/// A credential held in memory: a client secret, a token, a password.
///
/// It can be read only through [`Secret::expose`], which is the grep an auditor runs to find
/// every place a credential can escape. There is no `Display`, no `Serialize` and no
/// `Deref`, because each of those is a way for the value to reach a log or a response by
/// accident, and the buffer is wiped when the value is dropped.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wraps a plaintext credential the process already holds, from the environment or a file.
    pub fn new(plaintext: impl Into<String>) -> Self {
        Self(plaintext.into())
    }

    /// The plaintext. Every call site of this is a place where a credential can escape.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether the credential is empty, for the checks that must not read it.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl From<String> for Secret {
    fn from(plaintext: String) -> Self {
        Self::new(plaintext)
    }
}

#[cfg(test)]
mod tests {
    use super::Secret;

    /// SEC-GAP-03: the property is the absence of the value, not the presence of a word.
    #[test]
    fn no_formatting_of_a_secret_prints_it() {
        let secret = Secret::new("s3cr3t-value-nobody-may-log");
        for rendered in [
            format!("{secret:?}"),
            format!("{:?}", Some(secret.clone())),
            format!("{:?}", ("gateway-client", secret.clone())),
            format!("{:#?}", vec![secret.clone()]),
        ] {
            assert!(
                !rendered.contains("s3cr3t"),
                "a secret reached a formatted string: {rendered}",
            );
            assert!(rendered.contains("<redacted>"), "{rendered}");
        }
        // Asking for it by name is the one way out, so the value is still usable.
        assert_eq!(secret.expose(), "s3cr3t-value-nobody-may-log");
    }

    #[test]
    fn a_secret_is_still_a_value_that_compares_and_clones() {
        assert_eq!(Secret::new("a"), Secret::from("a".to_owned()));
        assert_ne!(Secret::new("a"), Secret::new("b"));
        assert!(Secret::new("").is_empty());
        assert!(!Secret::new("a").is_empty());
    }
}
