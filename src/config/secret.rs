//! A string that never reveals itself in `Debug` output (e.g. API tokens).

use serde::Deserialize;

#[derive(Clone, Deserialize)]
pub struct Secret(String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(\"***\")")
    }
}

impl Secret {
    /// Reveal the underlying secret. Call sites must never log the result.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacted_in_debug() {
        let s = Secret("super-secret-token".to_string());
        let rendered = format!("{s:?}");
        assert_eq!(rendered, "Secret(\"***\")");
        assert!(!rendered.contains("super-secret-token"));
    }
}
