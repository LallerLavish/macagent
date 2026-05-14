//! Parse LLM output into (headline, body).
//!
//! Handles three shapes transparently so swapping LoRA adapters
//! does not require any code change here:
//!   1. Structured:  "HEADLINE: ...\nDETAILS: ..."
//!   2. Markdown:    "**HEADLINE:** ...\n**DETAILS:** ..."
//!   3. Raw blob:    fallback — first sentence becomes headline.

pub fn parse_summary(raw: &str) -> (String, String) {
    let trimmed = raw.trim();
    if let Some(parsed) = try_parse_structured(trimmed) {
        return parsed;
    }
    split_first_sentence(trimmed)
}

fn try_parse_structured(s: &str) -> Option<(String, String)> {
    let lower = s.to_lowercase();
    let headline_idx = lower.find("headline:")?;
    let details_idx = lower.find("details:")?;

    if details_idx <= headline_idx {
        return None;
    }

    let headline_start = headline_idx + "headline:".len();
    let headline_raw = &s[headline_start..details_idx];
    let details_start = details_idx + "details:".len();
    let details_raw = &s[details_start..];

    let headline = strip_markdown(headline_raw).trim().to_string();
    let details = strip_markdown(details_raw).trim().to_string();

    if headline.is_empty() || details.is_empty() {
        return None;
    }

    Some((headline, details))
}

fn strip_markdown(s: &str) -> String {
    s.replace("**", "")
        .trim_matches(|c: char| c == '*' || c == ' ' || c == '\n')
        .to_string()
}

fn split_first_sentence(s: &str) -> (String, String) {
    let bytes = s.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if matches!(b, b'.' | b'!' | b'?') {
            let next = bytes.get(i + 1).copied().unwrap_or(b' ');
            if next == b' ' || next == b'\n' || i + 1 == bytes.len() {
                let headline = s[..=i].trim().to_string();
                let body = s[i + 1..].trim().to_string();
                if !headline.is_empty() {
                    return (
                        headline,
                        if body.is_empty() { s.to_string() } else { body },
                    );
                }
            }
        }
    }
    let h = s.trim().to_string();
    (h.clone(), h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_format() {
        let raw = "HEADLINE: Added baseline persistence.\nDETAILS: Saved git HEAD per branch.";
        let (h, b) = parse_summary(raw);
        assert_eq!(h, "Added baseline persistence.");
        assert_eq!(b, "Saved git HEAD per branch.");
    }

    #[test]
    fn structured_with_markdown() {
        let raw = "**HEADLINE:** Fix retrieval bug.\n**DETAILS:** Branch filter was missing.";
        let (h, b) = parse_summary(raw);
        assert_eq!(h, "Fix retrieval bug.");
        assert_eq!(b, "Branch filter was missing.");
    }

    #[test]
    fn raw_blob_falls_back() {
        let raw = "Refactored repos.rs to return DetectedRepo. Added branch state enum.";
        let (h, b) = parse_summary(raw);
        assert_eq!(h, "Refactored repos.rs to return DetectedRepo.");
        assert_eq!(b, "Added branch state enum.");
    }

    #[test]
    fn single_sentence_duplicates() {
        let raw = "Working tree is clean";
        let (h, b) = parse_summary(raw);
        assert_eq!(h, "Working tree is clean");
        assert_eq!(b, "Working tree is clean");
    }
}