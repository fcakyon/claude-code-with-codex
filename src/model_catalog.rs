//! Immutable model catalogues shared by routing, validation, and discovery.
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct ModelCatalog {
    models: BTreeMap<String, Vec<String>>,
}

impl Default for ModelCatalog {
    fn default() -> Self {
        Self {
            models: BTreeMap::from([
                (
                    "codex".into(),
                    crate::registry::CODEX_MODELS
                        .iter()
                        .map(|s| s.to_string())
                        .collect(),
                ),
                ("kimi".into(), vec!["kimi-for-coding".into(), "k3".into()]),
                (
                    "grok".into(),
                    crate::registry::GROK_MODELS
                        .iter()
                        .map(|s| s.to_string())
                        .collect(),
                ),
            ]),
        }
    }
}

impl ModelCatalog {
    pub fn replace(&mut self, provider: &str, mut models: Vec<String>) {
        models.sort_unstable();
        models.dedup();
        self.models.insert(provider.into(), models);
    }

    pub fn contains(&self, provider: &str, model: &str) -> bool {
        self.models
            .get(provider)
            .is_some_and(|models| models.iter().any(|m| m == model))
    }

    pub fn models(&self, provider: &str) -> Vec<String> {
        self.models.get(provider).cloned().unwrap_or_default()
    }

    pub fn listed_models(&self, provider: &str) -> Vec<String> {
        let mut models = self.models(provider);
        if provider == "codex" {
            models.extend(self.models(provider).iter().map(|m| format!("{m}-fast")));
        }
        if provider == "kimi" {
            // Compatibility names resolve to the original backend targets.
            models.extend(crate::registry::KIMI_MODELS.iter().map(|s| s.to_string()));
        }
        models.sort_unstable();
        models.dedup();
        models
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::AliasProvider, registry::Registry};
    use std::sync::Arc;

    #[test]
    fn discovered_models_drive_routing_validation_and_fast_aliases() {
        let mut catalog = ModelCatalog::default();
        catalog.replace("codex", vec!["gpt-toy-new".into()]);
        catalog.replace("kimi", vec!["kimi-toy-new".into()]);
        catalog.replace("grok", vec!["grok-toy-new".into()]);
        let catalog = Arc::new(catalog);
        let registry = Registry::with_catalog(AliasProvider::Anthropic, catalog.clone());
        for (model, provider) in [
            ("gpt-toy-new", "codex"),
            ("gpt-toy-new-fast", "codex"),
            ("kimi-toy-new", "kimi"),
            ("grok-toy-new", "grok"),
            ("claude-toy", "anthropic"),
            ("cursor:gpt-toy-new", "cursor"),
        ] {
            assert_eq!(
                registry.provider_for_model(model, None).unwrap().name(),
                provider
            );
        }
        assert!(registry.provider_for_model("gpt-5.4", None).is_none());
        let resolved =
            crate::providers::codex::translate::model_allowlist::resolve_model_with_catalog(
                "gpt-toy-new-fast",
                false,
                &catalog,
            );
        assert_eq!(resolved.model, "gpt-toy-new");
        assert_eq!(
            resolved.service_tier,
            Some(crate::providers::codex::translate::request::ServiceTier::Priority)
        );
        assert!(
            crate::providers::codex::translate::model_allowlist::assert_allowed_with_catalog(
                "gpt-toy-new",
                &catalog
            )
            .is_ok()
        );
        assert!(
            crate::providers::grok::translate::model_allowlist::assert_allowed_with_catalog(
                "grok-toy-new",
                &catalog
            )
            .is_ok()
        );
        assert_eq!(
            crate::providers::kimi::translate::model_allowlist::resolve_model_with_catalog(
                "kimi-toy-new",
                &catalog
            ),
            "kimi-toy-new"
        );
    }
}
