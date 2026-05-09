// src/pipeline/lexer.rs
//
// Lexer v2: classifies words into typed tokens with typo tolerance and
// word-boundary splitting.
//
// Major changes from v1:
//
//   1. Fuzzy verb matching. Verbs of 4+ characters get Levenshtein-1 typo
//      tolerance ("opne" → open, "trun" → turn, "clsoe" → close).
//      Shorter verbs ("set") require exact match.
//
//   2. Word-boundary splitting. "opensafari" → [Verb(Open), Word("safari")].
//      We try to split a Word token if it begins with a known verb prefix
//      and the remainder is non-trivial.
//
//   3. Typo tolerance for "except"/"but" (Levenshtein-1).
//      DELIBERATELY NOT applied to modifiers (on/off/up/down) or
//      quantifiers under 4 chars — false positive risk on real words is
//      too high. "of bluetooth" stays as Word("of") + Word("bluetooth")
//      and the grammar parser fails cleanly.
//
//   4. Confidence tracking. Each Token::Verb / Token::Quantifier carries
//      a flag indicating whether it was an exact match or typo-corrected.
//      The grammar parser uses this to lower the parse confidence of
//      typo-corrected commands.

use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Verb(VerbKind, bool),
    Modifier(ModifierKind),
    Conjunction,
    Quantifier(QuantifierKind, bool),
    Exception,
    Number(i32),
    /// Literal word "mode" or "modes" — signals that the surrounding
    /// target is a user-defined mode name, not a registry-resolvable target.
    ModeMarker,
    Word(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VerbKind {
    Open,
    Close,
    Turn,
    Set,
    Increase,
    Decrease,
    Toggle,
    Enable,
    Disable,
    Sleep,
    Restart,
    Shutdown,
    // Mode commands
    Save,
    Switch,
    ListVerb,   // "list" — named ListVerb to avoid conflict with std collections
    Delete,
    Leave,
    Debug,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModifierKind {
    On,
    Off,
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QuantifierKind {
    All,
    Every,
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Token::Verb(v, c) => write!(f, "Verb({:?}{})", v, if *c { "*" } else { "" }),
            Token::Modifier(m) => write!(f, "Mod({:?})", m),
            Token::Conjunction => write!(f, "Conj"),
            Token::Quantifier(q, c) => write!(f, "Quant({:?}{})", q, if *c { "*" } else { "" }),
            Token::Exception => write!(f, "Except"),
            Token::Number(n) => write!(f, "Num({})", n),
            Token::Word(w) => write!(f, "Word({:?})", w),
            Token::ModeMarker => write!(f, "ModeMarker"),

        }
    }
}

// ---- Verb spelling table -------------------------------------------------
//
// Every spelling of every verb (canonical form + synonyms). The lexer scans
// this table for both exact and fuzzy matching.

const VERB_SPELLINGS: &[(VerbKind, &[&str])] = &[
    (VerbKind::Open,     &["open", "launch", "start"]),
    (VerbKind::Close,    &["close", "quit", "exit", "kill"]),
    (VerbKind::Turn,     &["turn"]),
    (VerbKind::Set,      &["set"]),
    (VerbKind::Increase, &["increase", "raise"]),
    (VerbKind::Decrease, &["decrease", "lower", "reduce"]),
    (VerbKind::Toggle,   &["toggle"]),
    (VerbKind::Enable,   &["enable"]),
    (VerbKind::Disable,  &["disable"]),
    (VerbKind::Sleep,    &["sleep"]),
    (VerbKind::Restart,  &["restart", "reboot"]),
    (VerbKind::Shutdown, &["shutdown"]),
    // Mode verbs
    (VerbKind::Save,     &["save", "store"]),
    (VerbKind::Switch,   &["switch", "activate", "enter"]),
    (VerbKind::ListVerb, &["list", "show"]),
    (VerbKind::Delete,   &["delete", "remove"]),
    (VerbKind::Leave,    &["leave"]),
    (VerbKind::Debug, &["debug"]),
];

const MIN_FUZZY_VERB_LEN: usize = 4;
const MAX_VERB_EDIT_DISTANCE: usize = 1;

// ---- Tokenization entry point --------------------------------------------

pub fn tokenize(input: &str) -> Vec<Token> {
    eprintln!("[lexer] input: {:?}", input);
    const CONJ_SENTINEL: &str = " · ";

    // Pad input so trailing/leading conjunctions are reachable.
    let padded = format!(" {} ", input);

    let normalized = padded
        .replace(", and ", CONJ_SENTINEL)
        .replace(" and then ", CONJ_SENTINEL)
        .replace(", then ", CONJ_SENTINEL)
        .replace(" then ", CONJ_SENTINEL)
        .replace(" and ", CONJ_SENTINEL)
        .replace(';', CONJ_SENTINEL)
        .replace(',', CONJ_SENTINEL);

    let raw_words: Vec<&str> = normalized.split_whitespace().collect();
    let stripped = strip_fillers(&raw_words);

    let mut tokens = Vec::with_capacity(stripped.len());
    let mut i = 0;
    while i < stripped.len() {
        let word = stripped[i];

        if word == "·" {
            tokens.push(Token::Conjunction);
            i += 1;
            continue;
        }

        // Two-word verbs ("shut down")
        if i + 1 < stripped.len() {
            let two = format!("{} {}", word, stripped[i + 1]);
            if let Some(verb) = classify_two_word_verb(&two) {
                tokens.push(Token::Verb(verb, false));
                i += 2;
                continue;
            }
        }

        let lower = word.to_lowercase();

        // Try classification in this order:
        //   1. Exact verb (cheap, common case)
        //   2. Modifier (exact only — no fuzzy)
        //   3. Quantifier (exact + fuzzy for 4+ chars)
        //   4. Exception (exact + fuzzy for 4+ chars)
        //   5. Number
        //   6. Word-boundary split (verb prefix + target suffix)
        //   7. Fuzzy verb (last resort — only if nothing above matched)
        //   8. Word (everything else)

        if let Some(verb) = exact_verb(&lower) {
            tokens.push(Token::Verb(verb, false));
            i += 1;
            continue;
        }

        if let Some(modifier) = exact_modifier(&lower) {
            tokens.push(Token::Modifier(modifier));
            i += 1;
            continue;
        }

        if let Some((quant, corrected)) = classify_quantifier(&lower) {
            tokens.push(Token::Quantifier(quant, corrected));
            i += 1;
            continue;
        }

        if let Some(corrected) = classify_exception(&lower) {
            // We don't carry exception confidence on a separate token type,
            // but we expose it via the Exception variant. For simplicity v2
            // doesn't propagate the corrected flag on Exception — it's
            // rarely typo'd in practice.
            let _ = corrected;
            tokens.push(Token::Exception);
            i += 1;
            continue;
        }

        if let Some(n) = classify_number(&lower) {
            tokens.push(Token::Number(n));
            i += 1;
            continue;
        }
         if lower == "mode" || lower == "modes" {
            tokens.push(Token::ModeMarker);
            i += 1;
            continue;
        }
        // Word-boundary splitting: "opensafari" → [Verb(Open), Word("safari")]
        if let Some((verb, suffix)) = try_split_verb_prefix(&lower) {
            tokens.push(Token::Verb(verb, false));
            tokens.push(Token::Word(suffix));
            i += 1;
            continue;
        }

        // Fuzzy verb match (last-resort typo correction)
        if let Some(verb) = fuzzy_verb(&lower) {
            tokens.push(Token::Verb(verb, true));
            i += 1;
            continue;
        }

        tokens.push(Token::Word(lower));
        i += 1;
    }
    eprintln!("[lexer] output: {:?}", tokens);
    tokens
}

// ---- Filler stripping ----------------------------------------------------

const FILLER_PHRASES: &[&[&str]] = &[
    &["could", "you", "please"],
    &["can", "you", "please"],
    &["would", "you", "please"],
    &["please", "could", "you"],
    &["please", "can", "you"],
    &["could", "you"],
    &["can", "you"],
    &["would", "you"],
    &["for", "me"],
    &["right", "now"],
    &["just", "go"],
    &["go", "ahead", "and"],
];

const FILLER_WORDS: &[&str] = &[
    "please", "kindly", "hey", "ok", "okay", "alright",
    "now", "just", "really", "actually", "basically",
    "the", "a", "an",
    "my", "some",
];

fn strip_fillers<'a>(words: &[&'a str]) -> Vec<&'a str> {
    let lowered: Vec<String> = words.iter().map(|w| w.to_lowercase()).collect();
    let mut out = Vec::with_capacity(words.len());
    let mut i = 0;

    'outer: while i < words.len() {
        for phrase in FILLER_PHRASES {
            if i + phrase.len() <= lowered.len() {
                let slice = &lowered[i..i + phrase.len()];
                if slice.iter().zip(phrase.iter()).all(|(a, b)| a == b) {
                    i += phrase.len();
                    continue 'outer;
                }
            }
        }

        if FILLER_WORDS.contains(&lowered[i].as_str()) {
            i += 1;
            continue;
        }

        out.push(words[i]);
        i += 1;
    }

    out
}

// ---- Exact classification ------------------------------------------------

fn exact_verb(word: &str) -> Option<VerbKind> {
    for (kind, spellings) in VERB_SPELLINGS {
        if spellings.contains(&word) {
            return Some(*kind);
        }
    }
    None
}

fn exact_modifier(word: &str) -> Option<ModifierKind> {
    Some(match word {
        "on" => ModifierKind::On,
        "off" => ModifierKind::Off,
        "up" => ModifierKind::Up,
        "down" => ModifierKind::Down,
        _ => return None,
    })
}

fn classify_two_word_verb(phrase: &str) -> Option<VerbKind> {
    match phrase {
        "shut down" => Some(VerbKind::Shutdown),
        _ => None,
    }
}

/// Returns Some((kind, corrected)) where `corrected` is true if matched fuzzily.
fn classify_quantifier(word: &str) -> Option<(QuantifierKind, bool)> {
    let exact = match word {
        "all" => Some(QuantifierKind::All),
        "every" => Some(QuantifierKind::Every),
        _ => None,
    };
    if let Some(q) = exact {
        return Some((q, false));
    }

    // Fuzzy only for 4+ char tokens. "al" is too short to safely correct.
    if word.len() >= 4 {
        for (kind, spelling) in [
            (QuantifierKind::Every, "every"),
        ] {
            if levenshtein(word, spelling) <= 1 {
                return Some((kind, true));
            }
        }
    }
    None
}

/// Returns Some(corrected) where corrected is true if matched fuzzily.
fn classify_exception(word: &str) -> Option<bool> {
    if matches!(word, "except" | "but" | "besides") {
        return Some(false);
    }
    if word.len() >= 4 && levenshtein(word, "except") <= 1 {
        return Some(true);
    }
    None
}

fn classify_number(word: &str) -> Option<i32> {
    let trimmed = word.trim_end_matches('%');
    trimmed.parse::<i32>().ok()
}

// ---- Fuzzy verb matching -------------------------------------------------

fn fuzzy_verb(word: &str) -> Option<VerbKind> {
    if word.len() < MIN_FUZZY_VERB_LEN {
        return None;
    }

    let mut best: Option<(VerbKind, usize)> = None;
    for (kind, spellings) in VERB_SPELLINGS {
        for spelling in *spellings {
            // Skip short canonical spellings — symmetry with input length filter.
            if spelling.len() < MIN_FUZZY_VERB_LEN {
                continue;
            }
            // Length-difference shortcut: if lengths differ by more than the
            // edit-distance budget, no point computing.
            let ld = (spelling.len() as isize - word.len() as isize).unsigned_abs();
            if ld as usize > MAX_VERB_EDIT_DISTANCE {
                continue;
            }
            let dist = levenshtein(word, spelling);
            if dist <= MAX_VERB_EDIT_DISTANCE {
                match best {
                    None => best = Some((*kind, dist)),
                    Some((_, prev_d)) if dist < prev_d => best = Some((*kind, dist)),
                    _ => {}
                }
            }
        }
    }
    best.map(|(k, _)| k)
}

// ---- Word-boundary splitting ---------------------------------------------

/// If `word` starts with a known verb spelling and the remainder is at
/// least 2 chars (plausible target), return (verb, remainder).
///
/// This catches "opensafari", "closemail", "trunoff", etc. — typing
/// patterns where users miss the space.
fn try_split_verb_prefix(word: &str) -> Option<(VerbKind, String)> {
    // Try longest verb prefixes first to avoid false splits like
    // "opening" → "open" + "ing". We require the remainder to look like a
    // real target word (≥2 chars) and we ignore -ing / past-tense suffixes.
    let mut candidates: Vec<(usize, VerbKind, &str)> = Vec::new();
    for (kind, spellings) in VERB_SPELLINGS {
        for spelling in *spellings {
            if word.starts_with(spelling) && word.len() > spelling.len() + 1 {
                candidates.push((spelling.len(), *kind, spelling));
            }
        }
    }
    // Longest first.
    candidates.sort_by(|a, b| b.0.cmp(&a.0));

    for (prefix_len, kind, _) in candidates {
        let suffix = &word[prefix_len..];
        // Reject grammatical suffixes — "opening", "opened", "opens".
        if matches!(suffix, "ing" | "ed" | "s" | "er" | "ers") {
            continue;
        }
        // Reject if suffix starts with a vowel-then-consonant pattern that
        // looks like a continuation of the verb itself (false positive).
        // This is heuristic; safest tightening is "suffix must contain at
        // least one consonant cluster typical of a noun/proper noun." For
        // simplicity we accept any 2+ char suffix that isn't a known
        // grammatical ending.
        if suffix.len() < 2 {
            continue;
        }
        return Some((kind, suffix.to_string()));
    }
    None
}

// ---- Levenshtein (small, internal) --------------------------------------

/// Damerau-Levenshtein distance: counts adjacent character transpositions
/// as a single edit (instead of 2 in plain Levenshtein). This matches how
/// real typing errors happen — swapping two adjacent keys is the most
/// common typo, and we want it to count as 1 edit.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let n = a.len();
    let m = b.len();

    if n == 0 { return m; }
    if m == 0 { return n; }

    // Full 2D table — needed because transposition checks reach 2 rows back.
    let mut d = vec![vec![0usize; m + 1]; n + 1];
    for i in 0..=n { d[i][0] = i; }
    for j in 0..=m { d[0][j] = j; }

    for i in 1..=n {
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            let mut val = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);

            // Transposition: if the previous two chars are swapped versions
            // of each other, treat as 1 edit.
            if i > 1 && j > 1
                && a[i - 1] == b[j - 2]
                && a[i - 2] == b[j - 1]
            {
                val = val.min(d[i - 2][j - 2] + 1);
            }

            d[i][j] = val;
        }
    }

    d[n][m]
}

// ---- Tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn lex(input: &str) -> Vec<Token> {
        tokenize(input)
    }

    fn word(s: &str) -> Token {
        Token::Word(s.to_string())
    }

    fn vexact(v: VerbKind) -> Token {
        Token::Verb(v, false)
    }

    fn vfuzzy(v: VerbKind) -> Token {
        Token::Verb(v, true)
    }

    // ---- Existing behavior preserved -------------------------------------

    #[test]
    fn open_safari() {
        assert_eq!(lex("open Safari"), vec![vexact(VerbKind::Open), word("safari")]);
    }

    #[test]
    fn turn_on_wifi_and_bluetooth() {
        assert_eq!(
            lex("turn on wifi and bluetooth"),
            vec![
                vexact(VerbKind::Turn),
                Token::Modifier(ModifierKind::On),
                word("wifi"),
                Token::Conjunction,
                word("bluetooth"),
            ]
        );
    }

    #[test]
    fn shut_down() {
        assert_eq!(lex("shut down"), vec![vexact(VerbKind::Shutdown)]);
    }

    #[test]
    fn empty() {
        assert_eq!(lex(""), vec![]);
    }

    // ---- Fuzzy verb tests -----------------------------------------------

    #[test]
    fn fuzzy_open_typo() {
        assert_eq!(lex("opne safari"), vec![vfuzzy(VerbKind::Open), word("safari")]);
    }

    #[test]
    fn fuzzy_close_typo() {
        assert_eq!(lex("clsoe mail"), vec![vfuzzy(VerbKind::Close), word("mail")]);
    }

    #[test]
    fn fuzzy_turn_typo() {
        assert_eq!(
            lex("trun on wifi"),
            vec![vfuzzy(VerbKind::Turn), Token::Modifier(ModifierKind::On), word("wifi")]
        );
    }

    #[test]
    fn fuzzy_too_short_no_correction() {
        // "set" is 3 chars — fuzzy disabled. "ste" stays as a word.
        assert_eq!(lex("ste volume to 50"),
            vec![word("ste"), word("volume"), word("to"), Token::Number(50)]);
    }

    #[test]
    fn fuzzy_two_edits_no_correction() {
        // "opnn" is 2 edits from "open" — too far, stays as Word.
        let t = lex("opnn safari");
        assert!(matches!(t.first(), Some(Token::Word(_))));
    }

    // ---- Modifier / quantifier exact-only --------------------------------

    #[test]
    fn modifier_off_not_corrected_from_of() {
        // "of" is a real word — must NOT correct to "off".
        let t = lex("open list of files");
        // "of" should be Word, not Modifier(Off).
        assert!(t.iter().any(|tok| matches!(tok, Token::Word(w) if w == "of")));
        assert!(!t.iter().any(|tok| matches!(tok, Token::Modifier(ModifierKind::Off))));
    }

    #[test]
    fn quantifier_exact() {
        let t = lex("close all apps");
        assert!(t.contains(&Token::Quantifier(QuantifierKind::All, false)));
    }

    #[test]
    fn quantifier_short_typo_not_corrected() {
        // "al" is too short — stays a Word.
        let t = lex("close al apps");
        assert!(t.iter().any(|tok| matches!(tok, Token::Word(w) if w == "al")));
    }

    // ---- Word-boundary splitting -----------------------------------------

    #[test]
    fn split_opensafari() {
        assert_eq!(lex("opensafari"), vec![vexact(VerbKind::Open), word("safari")]);
    }

    #[test]
    fn split_closemail() {
        assert_eq!(lex("closemail"), vec![vexact(VerbKind::Close), word("mail")]);
    }

    #[test]
    fn split_does_not_match_opening() {
        // "opening" must not split into "open" + "ing"
        let t = lex("opening hours");
        assert!(matches!(t.first(), Some(Token::Word(w)) if w == "opening"));
    }

    // ---- Filler stripping still works -----------------------------------

    #[test]
    fn polite_command() {
        assert_eq!(
            lex("could you please open vs code"),
            vec![vexact(VerbKind::Open), word("vs"), word("code")]
        );
    }

    // ---- Order-independence support: lexer produces uniform stream -------
    //
    // The lexer doesn't enforce order — it just classifies. These tests
    // confirm the same set of tokens is produced regardless of input order.

    #[test]
    fn order_turn_off_bluetooth() {
        let t = lex("turn off bluetooth");
        assert!(t.contains(&vexact(VerbKind::Turn)));
        assert!(t.contains(&Token::Modifier(ModifierKind::Off)));
        assert!(t.contains(&word("bluetooth")));
    }

    #[test]
    fn order_turn_bluetooth_off() {
        let t = lex("turn bluetooth off");
        assert!(t.contains(&vexact(VerbKind::Turn)));
        assert!(t.contains(&Token::Modifier(ModifierKind::Off)));
        assert!(t.contains(&word("bluetooth")));
    }

    #[test]
    fn order_bluetooth_off_no_verb() {
        let t = lex("bluetooth off");
        assert!(t.contains(&Token::Modifier(ModifierKind::Off)));
        assert!(t.contains(&word("bluetooth")));
        assert!(!t.iter().any(|x| matches!(x, Token::Verb(_, _))));
    }
}