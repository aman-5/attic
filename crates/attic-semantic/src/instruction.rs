//! Versioned instruction templates for semantic query retrieval (Master Plan V2 §31).
//!
//! Query instructions adapt a general-purpose embedding model specifically
//! for asymmetric code and documentation retrieval. Centralizing them prevents
//! subtle drift across MCP handlers and CLI search endpoints.

/// Canonical instruction identifier for V1 code retrieval.
pub const CODE_RETRIEVAL_V1_ID: &str = "code_retrieval_v1";

/// Canonical query instruction template for V1 code retrieval.
pub const CODE_RETRIEVAL_V1_TEMPLATE: &str =
    "Instruct: Given a code search query, retrieve relevant code snippets and documentation\nQuery: ";

/// Format a search query string with the centralized retrieval instruction.
/// Documents are never formatted with instructions (document/query distinction).
pub fn format_query_instruction(instruction_id: &str, query: &str) -> String {
    match instruction_id {
        CODE_RETRIEVAL_V1_ID => format!("{CODE_RETRIEVAL_V1_TEMPLATE}{query}"),
        _ => query.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_query_appends_template_for_known_id() {
        let q = "find connection pool timeout";
        let formatted = format_query_instruction(CODE_RETRIEVAL_V1_ID, q);
        assert_eq!(
            formatted,
            format!("Instruct: Given a code search query, retrieve relevant code snippets and documentation\nQuery: {q}")
        );
    }

    #[test]
    fn format_query_returns_raw_for_unknown_id() {
        let q = "find connection pool timeout";
        let formatted = format_query_instruction("none", q);
        assert_eq!(formatted, q);
    }
}
