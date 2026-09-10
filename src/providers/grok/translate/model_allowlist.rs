pub fn resolve_model(model: &str) -> String {
    model.to_string()
}

pub fn assert_allowed_model(model: &str) -> anyhow::Result<()> {
    assert_allowed_with_catalog(model, &crate::model_catalog::ModelCatalog::default())
}

pub fn assert_allowed_with_catalog(
    model: &str,
    catalog: &crate::model_catalog::ModelCatalog,
) -> anyhow::Result<()> {
    if catalog.contains("grok", model) {
        Ok(())
    } else {
        anyhow::bail!("unsupported Grok model")
    }
}
