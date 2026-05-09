// src/registry/matcher.rs
//
// Fuzzy matching for user-typed queries against canonical names.
//
// Strategy: compute multiple scores and take the max. Each scoring strategy
// catches a different way users abbreviate or misspell things:
//
//   1. Exact match (case-insensitive)         → 1.0
//   2. Substring match                         → 0.85–0.95
//   3. Token-prefix match                      → 0.75–0.90
//   4. Acronym match (initials of words)       → 0.65–0.85
//   5. Levenshtein-based similarity            → 0.0–0.95
//
// Scoring is deliberately conservative — we'd rather return "no match" or
// "ambiguous" than confidently pick the wrong app.
//
// Thresholds (tunable; defaults are starting points based on sketched
// examples — adjust after seeing real misses):
//   ≥ 0.70  → Confident
//   ≥ 0.50  → Ambiguous candidate
//   < 0.50  → Discarded

const CONFIDENT_THRESHOLD: f32 = 0.70;
const CANDIDATE_THRESHOLD: f32 = 0.50;
const AMBIGUITY_GAP: f32 = 0.10; // top match must beat #2 by this much to be Confident

/// Result of resolving a user query against a registry.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolutionResult {
    /// One clear winner.
    Confident { canonical: String, score: f32 },
    /// Multiple candidates within striking distance — caller should ask the
    /// user or, in headless mode, take the highest as a hint with low confidence.
    Ambiguous { candidates: Vec<(String, f32)> },
    /// No candidate cleared the minimum bar.
    NotFound,
}

/// Score a single (query, candidate) pair on [0.0, 1.0].
///
/// Pure function — easy to unit test. All the algorithm complexity lives
/// here so resolve() stays simple.
pub fn score_match(query: &str, candidate: &str) -> f32 {
    let q = query.trim().to_lowercase();
    let c = candidate.trim().to_lowercase();

    if q.is_empty() || c.is_empty() {
        return 0.0;
    }

    // Strategy 1: exact match
    if q == c {
        return 1.0;
    }

    let mut best: f32 = 0.0;

    // Strategy 2: substring match
    // "code" appears in "Visual Studio Code" → 0.85
    // Score scales with how much of the candidate the substring covers, so
    // "vs code" matching "Visual Studio Code" beats "code" matching it.
    if c.contains(&q) {
        let coverage = q.len() as f32 / c.len() as f32;
        best = best.max(0.80 + 0.15 * coverage);
    }
   
    // Strategy 3: token-prefix match
    // Each token in the query is a prefix of a token in the candidate, in order.
    // "vs code" → "Visual Studio Code" because "vs" prefixes "visual studio"-as-tokens
    // (handled in the acronym strategy actually) but "vis stu cod" hits this.
    if let Some(score) = token_prefix_score(&q, &c) {
        best = best.max(score);
    }

    // Strategy 4: acronym match
    // "vsc" matches "Visual Studio Code" (initials). "vs code" matches it
    // because "vs" is treated as initials of the first two tokens and
    // "code" is a token match for the third.
    if let Some(score) = acronym_score(&q, &c) {
        best = best.max(score);
    }

    // Strategy 5: Levenshtein-based similarity
    // Catches typos: "saffari" → "Safari", "calcualtor" → "Calculator"
    // Only meaningful when lengths are comparable; otherwise the edit
    // distance is dominated by length difference.
    let lev = levenshtein_similarity(&q, &c);
    best = best.max(lev);

    best.clamp(0.0, 1.0)
}

/// Resolve a query against a list of canonical names.
///
/// Returns the strongest candidates and classifies the result.
pub fn resolve(query: &str, candidates: &[String]) -> ResolutionResult {
    if candidates.is_empty() {
        return ResolutionResult::NotFound;
    }

    let mut scored: Vec<(String, f32)> = candidates
        .iter()
        .map(|c| (c.clone(), score_match(query, c)))
        .filter(|(_, s)| *s >= CANDIDATE_THRESHOLD)
        .collect();

    // Sort descending by score
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    match scored.as_slice() {
        [] => ResolutionResult::NotFound,
        [(canonical, score)] if *score >= CONFIDENT_THRESHOLD => {
            ResolutionResult::Confident {
                canonical: canonical.clone(),
                score: *score,
            }
        }
        [(canonical, score)] => ResolutionResult::Ambiguous {
            candidates: vec![(canonical.clone(), *score)],
        },
        [(top_name, top_score), (_, second_score), ..]
            if *top_score >= CONFIDENT_THRESHOLD
                && (*top_score - *second_score) >= AMBIGUITY_GAP =>
        {
            ResolutionResult::Confident {
                canonical: top_name.clone(),
                score: *top_score,
            }
        }
        _ => {
            // Either top is below confident threshold, or runner-up is too close.
            // Cap candidate list at 5 to avoid spamming the UI.
            ResolutionResult::Ambiguous {
                candidates: scored.into_iter().take(5).collect(),
            }
        }
    }
}

// ---- Scoring strategy implementations ------------------------------------

/// Tokens are runs of alphanumerics. Punctuation and whitespace are separators.
fn tokenize(s: &str) -> Vec<&str> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect()
}

fn token_prefix_score(query: &str, candidate: &str) -> Option<f32> {
    let q_tokens = tokenize(query);
    let c_tokens = tokenize(candidate);

    if q_tokens.is_empty() || c_tokens.is_empty() {
        return None;
    }

    // Each query token must be a prefix of some candidate token, in order.
    // Track candidate position so order is preserved.
    let mut c_idx = 0;
    let mut matched_chars = 0usize;
    let mut total_q_chars = 0usize;

    for q_tok in &q_tokens {
        total_q_chars += q_tok.len();
        let mut found = false;
        while c_idx < c_tokens.len() {
            if c_tokens[c_idx].starts_with(q_tok) {
                matched_chars += q_tok.len();
                c_idx += 1;
                found = true;
                break;
            }
            c_idx += 1;
        }
        if !found {
            return None;
        }
    }

    // Score: how thoroughly did query tokens cover candidate tokens?
    let token_coverage = q_tokens.len() as f32 / c_tokens.len() as f32;
    let char_coverage = matched_chars as f32 / total_q_chars.max(1) as f32;
    Some(0.70 + 0.20 * token_coverage * char_coverage)
}

fn acronym_score(query: &str, candidate: &str) -> Option<f32> {
    let q = query.replace(|c: char| !c.is_alphanumeric(), "");
    let c_tokens = tokenize(candidate);
    if q.is_empty() || c_tokens.is_empty() {
        return None;
    }

    let initials: String = c_tokens
        .iter()
        .filter_map(|t| t.chars().next())
        .collect();

    // Pure acronym hit: "vsc" → "Visual Studio Code" initials are "vsc"
    if q == initials {
        return Some(0.85);
    }

    // Partial acronym: query is prefix of initials ("vs" → "vsc...")
    if !q.is_empty() && initials.starts_with(&q) {
        let coverage = q.len() as f32 / initials.len() as f32;
        return Some(0.65 + 0.15 * coverage);
    }

    // Acronym-with-suffix-token: "vs code" → first 2 chars match initials of
    // first 2 tokens, "code" matches last token directly.
    let q_parts: Vec<&str> = query.split_whitespace().collect();
    if q_parts.len() >= 2 {
        let prefix = q_parts[0];
        let rest = &q_parts[1..];

        if c_tokens.len() >= prefix.len() + rest.len() {
            let head_initials: String = c_tokens
                .iter()
                .take(prefix.len())
                .filter_map(|t| t.chars().next())
                .collect();

            if head_initials == prefix {
                let tail_tokens = &c_tokens[prefix.len()..];
                if rest.len() <= tail_tokens.len()
                    && rest
                        .iter()
                        .zip(tail_tokens.iter())
                        .all(|(r, t)| t.starts_with(r))
                {
                    return Some(0.82);
                }
            }
        }
    }

    // Concatenated-prefix match: "vscode" → "visual studio code"
    // Try to consume the query character-by-character, walking through the
    // candidate's tokens and allowing each token to consume 1+ characters.
    if let Some(score) = concatenated_prefix_score(&q, &c_tokens) {
        return Some(score);
    }

    None
}

/// Greedy: walk through candidate tokens, consuming as many query characters
/// as match the start of each token. Every token must contribute at least
/// one matching character. The query must be fully consumed.
///
/// "vscode" against ["visual", "studio", "code"]:
///   "visual" consumes "v" → query remaining: "scode"
///   "studio" consumes "s" → query remaining: "code"
///   "code"   consumes "code" → query remaining: ""
///   ✓ all consumed, all tokens contributed → match
fn concatenated_prefix_score(query: &str, c_tokens: &[&str]) -> Option<f32> {
    let q_chars: Vec<char> = query.chars().collect();
    let mut q_idx = 0usize;
    let mut tokens_used = 0usize;

    for token in c_tokens {
        if q_idx >= q_chars.len() {
            break;
        }
        let token_chars: Vec<char> = token.chars().collect();
        let mut consumed = 0usize;
        while consumed < token_chars.len()
            && q_idx < q_chars.len()
            && token_chars[consumed].to_ascii_lowercase()
                == q_chars[q_idx].to_ascii_lowercase()
        {
            consumed += 1;
            q_idx += 1;
        }
        if consumed == 0 {
            return None; // every token must contribute
        }
        tokens_used += 1;
    }

    if q_idx != q_chars.len() {
        return None; // didn't consume the whole query
    }

    // Score: prefer matches that use all tokens (tighter fit).
    let token_coverage = tokens_used as f32 / c_tokens.len() as f32;
    Some(0.75 + 0.15 * token_coverage)
}

fn levenshtein_similarity(a: &str, b: &str) -> f32 {
    let dist = levenshtein(a, b);
    let max_len = a.chars().count().max(b.chars().count()).max(1);
    1.0 - (dist as f32 / max_len as f32)
}

/// Standard Levenshtein distance. O(n*m) time and O(min(n,m)) space.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();

    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }

    // Use the shorter string as the inner dimension to minimize allocation.
    let (a, b) = if a.len() < b.len() { (b, a) } else { (a, b) };

    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr: Vec<usize> = vec![0; b.len() + 1];

    for i in 1..=a.len() {
        curr[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = (curr[j - 1] + 1)
                .min(prev[j] + 1)
                .min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[b.len()]
}

// ---- Tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<String> {
        [
            "Safari",
            "Visual Studio Code",
            "Calculator",
            "Calendar",
            "Chess",
            "Clock",
            "Mail",
            "Google Chrome",
            "1Password 7",
            "Microsoft Word",
            "Final Cut Pro",
            "DaVinci Resolve",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    fn assert_resolves_to(query: &str, expected: &str) {
        let r = resolve(query, &names());
        match r {
            ResolutionResult::Confident { canonical, .. } => {
                assert_eq!(canonical, expected, "query: {:?}", query);
            }
            other => panic!("expected Confident({:?}) for {:?}, got {:?}", expected, query, other),
        }
    }

    #[test]
    fn exact_match() {
        assert_resolves_to("Safari", "Safari");
    }

    #[test]
    fn case_insensitive() {
        assert_resolves_to("safari", "Safari");
        assert_resolves_to("SAFARI", "Safari");
    }

    #[test]
    fn substring() {
        assert_resolves_to("chrome", "Google Chrome");
    }

    #[test]
    fn typo_safari() {
        assert_resolves_to("saffari", "Safari");
    }

    #[test]
    fn typo_calculator() {
        assert_resolves_to("calcualtor", "Calculator");
    }

    #[test]
    fn acronym_vsc() {
        assert_resolves_to("vsc", "Visual Studio Code");
    }

    #[test]
    fn vs_code() {
        assert_resolves_to("vs code", "Visual Studio Code");
    }

    #[test]
    fn vscode_no_space() {
        // "vscode" should resolve to Visual Studio Code via either substring
        // (after space-stripping) or Levenshtein. Either Confident or
        // Ambiguous-with-VS-Code-as-top is acceptable.
        let r = resolve("vscode", &names());
        match r {
            ResolutionResult::Confident { canonical, .. } => {
                assert_eq!(canonical, "Visual Studio Code");
            }
            ResolutionResult::Ambiguous { candidates } => {
                assert_eq!(
                    candidates.first().map(|c| c.0.as_str()),
                    Some("Visual Studio Code"),
                    "expected Visual Studio Code as top candidate"
                );
            }
            ResolutionResult::NotFound => {
                panic!("vscode should resolve, got NotFound");
            }
        }
    }

    #[test]
    fn calc() {
        assert_resolves_to("calc", "Calculator");
    }

    #[test]
    fn nonsense_returns_not_found() {
        let r = resolve("xyzzy_blarg", &names());
        assert_eq!(r, ResolutionResult::NotFound);
    }

    #[test]
    fn empty_query_not_found() {
        let r = resolve("", &names());
        assert_eq!(r, ResolutionResult::NotFound);
    }

    #[test]
    fn ambiguous_returns_candidates() {
        // "cal" prefixes Calculator and Calendar — should be ambiguous.
        let r = resolve("cal", &names());
        match r {
            ResolutionResult::Ambiguous { candidates } => {
                let names_only: Vec<&str> = candidates.iter().map(|c| c.0.as_str()).collect();
                assert!(names_only.contains(&"Calculator"));
                assert!(names_only.contains(&"Calendar"));
            }
            other => panic!("expected Ambiguous, got {:?}", other),
        }
    }

    #[test]
    fn final_cut() {
        assert_resolves_to("final cut", "Final Cut Pro");
    }

    #[test]
    fn fcp_acronym() {
        assert_resolves_to("fcp", "Final Cut Pro");
    }
}