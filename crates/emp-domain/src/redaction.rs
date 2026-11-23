use regex::Regex;
use std::sync::LazyLock;

static REDACTIONS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
    r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{6,}", r"(?i)\bsk-[A-Za-z0-9_-]{10,}\b",
    r"\bAIza[0-9A-Za-z_-]{20,}\b", r"(?i)\bgh[pousr]_[A-Za-z0-9]{20,}\b",
    r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{4,}\b",
    r#"(?i)\b(api[-_ ]?key|access[-_ ]?token|refresh[-_ ]?token|token|password|secret|set[-_ ]?cookie|cookie|authorization|bootstrap|session)\s*[=:]\s*(?:bearer\s+)?["']?[^\s"',;&{}]{4,}"#,
].into_iter().map(|pattern| Regex::new(pattern).unwrap()).collect()
});
pub fn redact_sensitive_text(text: &str) -> String {
    REDACTIONS.iter().fold(text.to_owned(), |text, pattern| {
        pattern.replace_all(&text, "<redacted>").into_owned()
    })
}
