//! Text projection for hosted searches completed by xAI.
//!
//! Hosted-tool block support varies across Anthropic clients. The text shape
//! preserves the search name and query using the baseline content type, while
//! citations and usage remain attached to the translated response.

/// One line naming a completed search. xAI returns findings as citations on the
/// answer text, so the search event itself has no result payload to include.
pub fn search_line(name: &str, query: &str) -> String {
    let label = match name {
        "x_search" => "X search",
        "web_search" => "web search",
        other => other,
    };
    let query = query.trim();
    if query.is_empty() {
        format!("[{label}]\n")
    } else {
        format!("[{label}: {query}]\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_map_to_readable_labels() {
        assert_eq!(search_line("web_search", "cars"), "[web search: cars]\n");
        assert_eq!(search_line("x_search", "outage"), "[X search: outage]\n");
    }

    #[test]
    fn an_unknown_hosted_tool_keeps_its_own_name() {
        assert_eq!(
            search_line("news_search", "budget"),
            "[news_search: budget]\n"
        );
    }

    #[test]
    fn an_empty_query_drops_the_colon() {
        // Grok emits follow-up search events with no query of their own.
        assert_eq!(search_line("web_search", "   "), "[web search]\n");
    }
}
