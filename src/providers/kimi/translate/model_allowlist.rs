use std::collections::HashMap;

pub const KIMI_DEFAULT_MODEL: &str = "kimi-for-coding";

static ALIAS_TARGETS: once_cell::sync::Lazy<HashMap<&'static str, &'static str>> =
    once_cell::sync::Lazy::new(|| {
        let mut m = HashMap::new();
        m.insert("haiku", KIMI_DEFAULT_MODEL);
        m.insert("claude-haiku-4-5", KIMI_DEFAULT_MODEL);
        m.insert("claude-haiku-4-5-20251001", KIMI_DEFAULT_MODEL);
        m.insert("sonnet", KIMI_DEFAULT_MODEL);
        m.insert("claude-sonnet-4-6", KIMI_DEFAULT_MODEL);
        m.insert("opus", KIMI_DEFAULT_MODEL);
        m.insert("claude-opus-4-7", KIMI_DEFAULT_MODEL);
        m.insert("kimi-for-coding", KIMI_DEFAULT_MODEL);
        m
    });

pub fn resolve_model(model: &str) -> String {
    ALIAS_TARGETS
        .get(model)
        .copied()
        .unwrap_or(KIMI_DEFAULT_MODEL)
        .to_string()
}

pub fn assert_allowed_model(model: &str) -> Result<(), ModelNotAllowedError> {
    if model != KIMI_DEFAULT_MODEL {
        return Err(ModelNotAllowedError {
            model: model.to_string(),
        });
    }
    Ok(())
}

#[derive(Debug)]
pub struct ModelNotAllowedError {
    pub model: String,
}

impl std::fmt::Display for ModelNotAllowedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Model not allowed: {}", self.model)
    }
}

impl std::error::Error for ModelNotAllowedError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_haiku_to_default() {
        assert_eq!(resolve_model("haiku"), KIMI_DEFAULT_MODEL);
    }

    #[test]
    fn resolve_unknown_to_default() {
        assert_eq!(resolve_model("unknown-model"), KIMI_DEFAULT_MODEL);
    }

    #[test]
    fn resolve_kimi_for_coding() {
        assert_eq!(resolve_model("kimi-for-coding"), KIMI_DEFAULT_MODEL);
    }

    #[test]
    fn assert_allowed_accepts_default() {
        assert!(assert_allowed_model(KIMI_DEFAULT_MODEL).is_ok());
    }

    #[test]
    fn assert_allowed_rejects_other() {
        assert!(assert_allowed_model("kimi-k2.6").is_err());
    }
}
