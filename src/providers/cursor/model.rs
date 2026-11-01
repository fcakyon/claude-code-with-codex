/// Cursor model catalog resolves incoming model names to Cursor wire IDs.
pub const CURSOR_LEGACY_MODELS: &[&str] = &[
    "cursor",
    "cursor-agent",
    "cursor-composer",
    "cursor-composer-fast",
    "cursor-plan",
    "cursor-ask",
    "composer-2.5",
    "composer-2.5-fast",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorAgentMode {
    Agent,
    Plan,
    Ask,
}

impl CursorAgentMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            CursorAgentMode::Agent => "AGENT_MODE_AGENT",
            CursorAgentMode::Plan => "AGENT_MODE_PLAN",
            CursorAgentMode::Ask => "AGENT_MODE_ASK",
        }
    }
}

/// Cursor list ids whose effort lives in parameters, not in the model id.
///
/// `claude-sonnet-5-5-medium` is not a wire id. The wire id is
/// `claude-sonnet-5-5` with `context=300k` and `reasoning_effort=medium`.
/// Sending `fast` on that model makes Cursor return ERROR_BAD_MODEL_NAME.
struct ParameterFamily {
    id: &'static str,
    effort_key: &'static str,
    default_effort: &'static str,
    context: Option<&'static str>,
    thinking: bool,
    send_fast: bool,
}

const PARAMETER_FAMILIES: &[ParameterFamily] = &[
    ParameterFamily {
        id: "claude-sonnet-5-5",
        effort_key: "reasoning_effort",
        default_effort: "high",
        context: Some("300k"),
        thinking: false,
        send_fast: false,
    },
    ParameterFamily {
        id: "claude-opus-5-5",
        effort_key: "effort",
        default_effort: "medium",
        context: Some("300k"),
        thinking: false,
        send_fast: true,
    },
    ParameterFamily {
        id: "claude-fable-5-1",
        effort_key: "effort",
        default_effort: "high",
        context: Some("300k"),
        thinking: true,
        send_fast: false,
    },
    ParameterFamily {
        id: "claude-sonnet-5",
        effort_key: "effort",
        default_effort: "high",
        context: Some("300k"),
        thinking: true,
        send_fast: false,
    },
    ParameterFamily {
        id: "claude-fable-5",
        effort_key: "effort",
        default_effort: "high",
        context: Some("300k"),
        thinking: true,
        send_fast: false,
    },
    ParameterFamily {
        id: "gemini-3.8-flash",
        effort_key: "reasoning_effort",
        default_effort: "high",
        context: None,
        thinking: false,
        send_fast: false,
    },
    ParameterFamily {
        id: "gemini-4-argon",
        effort_key: "reasoning_effort",
        default_effort: "medium",
        context: None,
        thinking: false,
        send_fast: false,
    },
    ParameterFamily {
        id: "muse-spark-1.3",
        effort_key: "effort",
        default_effort: "high",
        context: Some("300k"),
        thinking: false,
        send_fast: false,
    },
    ParameterFamily {
        id: "muse-wonder-1.0",
        effort_key: "reasoning_effort",
        default_effort: "high",
        context: Some("300k"),
        thinking: false,
        send_fast: false,
    },
    ParameterFamily {
        id: "kimi-k3",
        effort_key: "reasoning",
        default_effort: "max",
        context: None,
        thinking: false,
        send_fast: false,
    },
];

const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

struct ResolvedWire {
    model_id: String,
    fast: bool,
    parameters: Vec<(String, String)>,
    send_fast: bool,
}

fn is_effort(value: &str) -> bool {
    EFFORTS.contains(&value)
}

fn matches_family(raw: &str, id: &str) -> bool {
    raw == id
        || raw
            .strip_prefix(id)
            .is_some_and(|rest| rest.starts_with('-'))
}

fn find_family(raw: &str) -> Option<&'static ParameterFamily> {
    PARAMETER_FAMILIES
        .iter()
        .filter(|family| matches_family(raw, family.id))
        .max_by_key(|family| family.id.len())
}

/// Split a list id such as `claude-sonnet-5-5-medium` into the wire id and
/// the parameters Cursor's catalog actually stores on that variant.
fn parameterized_model(raw: &str) -> Option<ResolvedWire> {
    let family = find_family(raw)?;
    let rest = raw[family.id.len()..].trim_start_matches('-');
    let (thinking, effort) = if family.thinking {
        if let Some(after) = rest.strip_prefix("thinking") {
            let after = after.trim_start_matches('-');
            let effort = if after.is_empty() {
                family.default_effort
            } else {
                after
            };
            (Some("true"), effort)
        } else if rest.is_empty() {
            (Some("true"), family.default_effort)
        } else {
            (Some("false"), rest)
        }
    } else if rest.is_empty() {
        (None, family.default_effort)
    } else {
        (None, rest)
    };
    if !is_effort(effort) {
        return None;
    }
    let mut parameters = Vec::new();
    if let Some(thinking) = thinking {
        parameters.push(("thinking".to_string(), thinking.to_string()));
    }
    if let Some(context) = family.context {
        parameters.push(("context".to_string(), context.to_string()));
    }
    parameters.push((family.effort_key.to_string(), effort.to_string()));
    Some(ResolvedWire {
        model_id: family.id.to_string(),
        fast: false,
        parameters,
        send_fast: family.send_fast,
    })
}

fn resolve_wire_model(raw: &str) -> ResolvedWire {
    let (stripped, fast) = match raw.strip_suffix("-fast") {
        Some(model) => (model.to_string(), true),
        None => (raw.to_string(), false),
    };
    if let Some(mut resolved) = parameterized_model(&stripped) {
        resolved.fast = fast;
        return resolved;
    }
    ResolvedWire {
        model_id: stripped,
        fast,
        parameters: Vec::new(),
        send_fast: true,
    }
}

fn resolution(model_id: &str, mode: CursorAgentMode, fast: bool) -> CursorModelResolution {
    CursorModelResolution {
        model_id: model_id.to_string(),
        mode,
        fast,
        parameters: Vec::new(),
        send_fast: true,
    }
}

pub fn resolve_cursor_model(model: &str) -> Result<CursorModelResolution, String> {
    let model = model.trim();
    for (prefix, mode) in [
        ("cursor-agent:", CursorAgentMode::Agent),
        ("cursor-plan:", CursorAgentMode::Plan),
        ("cursor-ask:", CursorAgentMode::Ask),
        ("cursor:", CursorAgentMode::Agent),
    ] {
        if let Some(raw) = model.strip_prefix(prefix) {
            let resolved = resolve_wire_model(raw);
            return Ok(CursorModelResolution {
                model_id: resolved.model_id,
                mode,
                fast: resolved.fast,
                parameters: resolved.parameters,
                send_fast: resolved.send_fast,
            });
        }
    }

    let (model_id, mode, fast) = match model {
        "cursor" | "cursor-agent" => ("default", CursorAgentMode::Agent, false),
        "cursor-plan" => ("default", CursorAgentMode::Plan, false),
        "cursor-ask" => ("default", CursorAgentMode::Ask, false),
        "cursor-composer" | "composer-2.5" => ("composer-2.5", CursorAgentMode::Agent, false),
        "cursor-composer-fast" | "composer-2.5-fast" => {
            ("composer-2.5", CursorAgentMode::Agent, true)
        }
        _ => {
            return Err(format!(
                "unknown cursor model: {model}. Supported: cursor:<id>, cursor-plan:<id>, cursor-ask:<id>, cursor-agent"
            ));
        }
    };
    Ok(resolution(model_id, mode, fast))
}

#[derive(Debug, Clone)]
pub struct CursorModelResolution {
    pub model_id: String,
    pub mode: CursorAgentMode,
    pub fast: bool,
    /// Catalog parameters other than `fast`. `fast` stays on `send_fast`.
    pub parameters: Vec<(String, String)>,
    /// When false, the model has no `fast` parameter and sending one is a bad model name.
    pub send_fast: bool,
}

pub fn cursor_supported_models() -> Vec<String> {
    let mut out: Vec<String> = CURSOR_LEGACY_MODELS
        .iter()
        .map(|model| (*model).to_string())
        .collect();
    out.sort_unstable();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_legacy_cursor() {
        let resolution = resolve_cursor_model("cursor").unwrap();
        assert_eq!(resolution.model_id, "default");
        assert_eq!(resolution.mode, CursorAgentMode::Agent);
    }

    #[test]
    fn resolve_legacy_cursor_agent() {
        let resolution = resolve_cursor_model("cursor-agent").unwrap();
        assert_eq!(resolution.model_id, "default");
        assert_eq!(resolution.mode, CursorAgentMode::Agent);
    }

    #[test]
    fn resolve_legacy_cursor_plan() {
        let resolution = resolve_cursor_model("cursor-plan").unwrap();
        assert_eq!(resolution.model_id, "default");
        assert_eq!(resolution.mode, CursorAgentMode::Plan);
    }

    #[test]
    fn resolve_legacy_cursor_ask() {
        let resolution = resolve_cursor_model("cursor-ask").unwrap();
        assert_eq!(resolution.model_id, "default");
        assert_eq!(resolution.mode, CursorAgentMode::Ask);
    }

    #[test]
    fn resolve_prefixed_cursor() {
        let resolution = resolve_cursor_model("cursor:gpt-5.5").unwrap();
        assert_eq!(resolution.model_id, "gpt-5.5");
        assert_eq!(resolution.mode, CursorAgentMode::Agent);
    }

    #[test]
    fn resolve_prefixed_cursor_plan() {
        let resolution = resolve_cursor_model("cursor-plan:gpt-5.5").unwrap();
        assert_eq!(resolution.model_id, "gpt-5.5");
        assert_eq!(resolution.mode, CursorAgentMode::Plan);
    }

    #[test]
    fn resolve_prefixed_cursor_ask() {
        let resolution = resolve_cursor_model("cursor-ask:gpt-5.5").unwrap();
        assert_eq!(resolution.model_id, "gpt-5.5");
        assert_eq!(resolution.mode, CursorAgentMode::Ask);
    }

    #[test]
    fn resolve_prefixed_cursor_agent() {
        let resolution = resolve_cursor_model("cursor-agent:gpt-5.5").unwrap();
        assert_eq!(resolution.model_id, "gpt-5.5");
        assert_eq!(resolution.mode, CursorAgentMode::Agent);
    }

    #[test]
    fn resolve_unknown_model_errors() {
        assert!(resolve_cursor_model("unknown-model").is_err());
    }

    #[test]
    fn sonnet_5_5_medium_uses_catalog_parameters() {
        let resolution = resolve_cursor_model("cursor:claude-sonnet-5-5-medium").unwrap();
        assert_eq!(resolution.model_id, "claude-sonnet-5-5");
        assert!(!resolution.fast);
        assert!(!resolution.send_fast);
        assert_eq!(
            resolution.parameters,
            vec![
                ("context".to_string(), "300k".to_string()),
                ("reasoning_effort".to_string(), "medium".to_string()),
            ]
        );
    }

    #[test]
    fn opus_medium_fast_keeps_fast_and_effort() {
        let resolution = resolve_cursor_model("cursor:claude-opus-5-5-medium-fast").unwrap();
        assert_eq!(resolution.model_id, "claude-opus-5-5");
        assert!(resolution.fast);
        assert!(resolution.send_fast);
        assert!(
            resolution
                .parameters
                .iter()
                .any(|(key, value)| key == "effort" && value == "medium")
        );
        assert!(
            resolution
                .parameters
                .iter()
                .any(|(key, value)| key == "context" && value == "300k")
        );
    }

    #[test]
    fn fable_high_is_not_the_thinking_variant() {
        let plain = resolve_cursor_model("cursor:claude-fable-5-1-high").unwrap();
        assert_eq!(plain.model_id, "claude-fable-5-1");
        assert!(
            plain
                .parameters
                .iter()
                .any(|(key, value)| key == "thinking" && value == "false")
        );
        let thinking = resolve_cursor_model("cursor:claude-fable-5-1-thinking-high").unwrap();
        assert!(
            thinking
                .parameters
                .iter()
                .any(|(key, value)| key == "thinking" && value == "true")
        );
    }

    #[test]
    fn resolve_composer_models() {
        let regular = resolve_cursor_model("composer-2.5").unwrap();
        assert_eq!(regular.mode, CursorAgentMode::Agent);
        assert_eq!(regular.model_id, "composer-2.5");
        assert!(!regular.fast);

        let fast = resolve_cursor_model("composer-2.5-fast").unwrap();
        assert_eq!(fast.mode, CursorAgentMode::Agent);
        assert_eq!(fast.model_id, "composer-2.5");
        assert!(fast.fast);
    }

    #[test]
    fn supported_models_includes_all_legacy() {
        let models = cursor_supported_models();
        for model in CURSOR_LEGACY_MODELS {
            assert!(models.contains(&model.to_string()), "missing {model}");
        }
    }
}
