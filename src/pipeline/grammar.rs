// src/pipeline/grammar.rs
//
// Grammar parser v2: bag-of-tokens scanner.
//
// Major redesign from v1:
//   - No recursive descent. Each phrase is parsed as a bag-of-tokens:
//     scan for verbs, modifiers, numbers, quantifiers — extract them.
//     Whatever Word tokens remain become the target string.
//   - Order independence: "turn off bluetooth" and "turn bluetooth off"
//     and "off bluetooth" all produce the same parse result.
//   - Verb is now `Option<VerbKind>` — the resolver infers from context
//     when grammar can't determine it.
//   - Three-way inheritance across conjunctions:
//       a) phrase has only targets       → inherit verb AND modifier
//       b) phrase has modifier, no verb  → inherit verb, use new modifier
//       c) phrase has verb               → inherit nothing (reset modifier)
//   - "to" / "by" adjacent to numbers are stripped from target text.
//   - Parse confidence: lowered when verb was typo-corrected, when verb
//     was inferred (verb=None), or when target tokens were stitched
//     across non-target tokens (interleaved targets).

use std::fmt;

use super::lexer::{ModifierKind, QuantifierKind, Token, VerbKind};

#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    pub phrases: Vec<ActionPhrase>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ActionPhrase {
    /// May be None — resolver infers from target type.
    pub verb: Option<VerbKind>,
    pub modifier: Option<ModifierKind>,
    pub targets: TargetExpr,
    pub amount: Option<i32>,
    /// Parse confidence — combined with target match confidence in resolver.
    pub parse_confidence: ParseConfidence,
    pub is_mode_command: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseConfidence {
    /// Verb explicit and exactly spelled. Highest confidence.
    Exact,
    /// Verb typo-corrected, OR target stitched across interleaved tokens.
    Corrected,
    /// Verb inferred (was None — resolver will figure it out).
    Inferred,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TargetExpr {
    List(Vec<String>),
    Quantified {
        quantifier: QuantifierKind,
        class: String,
        except: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParseError {
    Empty,
    NoVerb,
    UnexpectedToken(String),
    DanglingConjunction,
    EmptyTarget,
    /// More than one verb or modifier in a single phrase.
    MultipleVerbs,
    MultipleModifiers,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Empty => write!(f, "empty command"),
            ParseError::NoVerb => write!(f, "no recognized action in command"),
            ParseError::UnexpectedToken(t) => write!(f, "unexpected token: {}", t),
            ParseError::DanglingConjunction => write!(f, "command ends with 'and' / ','"),
            ParseError::EmptyTarget => write!(f, "action has no target"),
            ParseError::MultipleVerbs => write!(f, "phrase contains more than one action verb"),
            ParseError::MultipleModifiers => write!(f, "phrase contains conflicting modifiers"),
        }
    }
}

impl std::error::Error for ParseError {}

// ---- Entry point ---------------------------------------------------------

pub fn parse(tokens: &[Token]) -> Result<Command, ParseError> {
    if tokens.is_empty() {
        return Err(ParseError::Empty);
    }

    // Step 1: split tokens into phrases by Conjunction. Reject dangling.
    let phrase_groups = split_into_phrases(tokens)?;

    // Step 2: parse each phrase group with inheritance from prior phrases.
    let mut phrases = Vec::with_capacity(phrase_groups.len());
    let mut last_verb: Option<VerbKind> = None;
    let mut last_modifier: Option<ModifierKind> = None;

    for group in phrase_groups {
        let phrase = parse_phrase_group(&group, last_verb, last_modifier)?;
        // Update inheritance state.
        last_verb = phrase.verb;
        last_modifier = phrase.modifier;
        phrases.push(phrase);
    }

    if phrases.is_empty() {
        return Err(ParseError::Empty);
    }

    Ok(Command { phrases })
}

// ---- Phrase splitting ----------------------------------------------------

fn split_into_phrases(tokens: &[Token]) -> Result<Vec<Vec<Token>>, ParseError> {
    let mut groups: Vec<Vec<Token>> = Vec::new();
    let mut current: Vec<Token> = Vec::new();

    for tok in tokens {
        match tok {
            Token::Conjunction => {
                if current.is_empty() {
                    // Conjunction with no preceding tokens — leading "and"
                    // or ",". Tolerate it by skipping.
                    continue;
                }
                groups.push(std::mem::take(&mut current));
            }
            other => current.push(other.clone()),
        }
    }

    // If current is non-empty, that's the final phrase.
    if !current.is_empty() {
        groups.push(current);
    } else if !tokens.is_empty()
        && matches!(tokens.last(), Some(Token::Conjunction))
        && !groups.is_empty()
    {
        // Input ended with a conjunction → dangling.
        return Err(ParseError::DanglingConjunction);
    }

    Ok(groups)
}

// ---- Per-phrase parsing --------------------------------------------------

fn parse_phrase_group(
    group: &[Token],
    inherited_verb: Option<VerbKind>,
    inherited_modifier: Option<ModifierKind>,
) -> Result<ActionPhrase, ParseError> {
    // Bag-of-tokens scan: pull out verbs, modifiers, quantifiers, numbers,
    // exceptions. Track verb/quantifier corrections for confidence.
    // Whatever's left is the target.

    let mut verb: Option<(VerbKind, bool)> = None; // (kind, corrected)
    let mut mode_marker_seen = false;
    let mut modifier: Option<ModifierKind> = None;
    let mut number: Option<i32> = None;
    let mut quantifier: Option<(QuantifierKind, bool)> = None;
    let mut exception_seen = false;

    // Track target tokens with their index so we can detect "interleaved"
    // patterns ("visual turn studio code on" — verb sits inside target).
    let mut target_token_indices: Vec<usize> = Vec::new();

    for (idx, tok) in group.iter().enumerate() {
        match tok {
            Token::Verb(v, corrected) => {
                if verb.is_some() {
                    return Err(ParseError::MultipleVerbs);
                }
                verb = Some((*v, *corrected));
            }
            
            Token::Modifier(m) => {
                if let Some(existing) = modifier {
                    if existing != *m {
                        return Err(ParseError::MultipleModifiers);
                    }
                    // Same modifier seen twice — tolerate.
                } else {
                    modifier = Some(*m);
                }
            }
            Token::Number(n) => {
                // For now, allow only one number per phrase; later numbers
                // overwrite. Could be MultipleNumbers, but in practice the
                // last number is what users mean ("set volume to 50 60").
                number = Some(*n);
            }
            Token::Quantifier(q, corrected) => {
                quantifier = Some((*q, *corrected));
                target_token_indices.push(idx); // class noun follows
            }
            Token::Exception => {
                exception_seen = true;
                target_token_indices.push(idx); // marker for splitting
            }
            Token::Word(_) => {
                target_token_indices.push(idx);
            }
            Token::ModeMarker => {
                mode_marker_seen = true;
                // Do NOT push to target_token_indices — "mode" is a marker,
                // not part of the mode name.
            }
            Token::Conjunction => {
                // Should never appear here — split_into_phrases removed them.
                return Err(ParseError::UnexpectedToken(format!("{}", tok)));
            }
        }
    }

    // ---- Build the target expression --------------------------------

    let targets = if let Some((q, _)) = quantifier {
        build_quantified_target(group, q, exception_seen)?
    } else {
        build_target_list(group, &target_token_indices, number)?
    };

    // ---- Resolve verb + modifier with three-way inheritance --------

    let (final_verb, final_modifier) = resolve_verb_inheritance(
        verb.map(|(v, _)| v),
        modifier,
        inherited_verb,
        inherited_modifier,
    );

    // ---- Determine parse confidence --------------------------------

    let interleaved = is_interleaved(group, &target_token_indices);
    let parse_confidence = compute_confidence(
        verb.map(|(_, c)| c).unwrap_or(false),
        verb.is_some(),
        interleaved,
    );

    // ---- Validation -----------------------------------------------

    // Empty target on a verb-only phrase: only OK for system-action verbs
    // (Sleep, Restart, Shutdown). Other verbs must have a target.
    if let Some(v) = final_verb {
            if verb_requires_target(v) && !is_no_target_mode_verb(v, mode_marker_seen) {
                match &targets {
                    TargetExpr::List(v) if v.is_empty() => {
                        return Err(ParseError::EmptyTarget);
                    }
                    _ => {}
                }
            }
        } else {
            match &targets {
                TargetExpr::List(v) if v.is_empty() && modifier.is_none() => {
                    return Err(ParseError::NoVerb);
                }
                _ => {}
            }
        }

    Ok(ActionPhrase {
        verb: final_verb,
        modifier: final_modifier,
        targets,
        amount: number,
        parse_confidence,
        is_mode_command: mode_marker_seen,
    })
}

// ---- Target construction -------------------------------------------------

fn build_target_list(
    group: &[Token],
    target_indices: &[usize],
    number: Option<i32>,
) -> Result<TargetExpr, ParseError> {
    if target_indices.is_empty() {
        return Ok(TargetExpr::List(vec![]));
    }

    // Stitch Word tokens at the target indices, in order, joined by spaces.
    // Skip "to"/"by" if a Number was extracted (they were prepositions for it).
    let mut words: Vec<String> = Vec::with_capacity(target_indices.len());
    for &i in target_indices {
        if let Some(Token::Word(w)) = group.get(i) {
            if number.is_some() && (w == "to" || w == "by") {
                continue;
            }
            words.push(w.clone());
        }
    }

    if words.is_empty() {
        return Ok(TargetExpr::List(vec![]));
    }

    Ok(TargetExpr::List(vec![words.join(" ")]))
}

fn build_quantified_target(
    group: &[Token],
    quantifier: QuantifierKind,
    exception_seen: bool,
) -> Result<TargetExpr, ParseError> {
    // Walk tokens; gather words after the Quantifier as the class, and
    // words after the Exception as the except-target.
    let mut class_words: Vec<String> = Vec::new();
    let mut except_words: Vec<String> = Vec::new();
    let mut state = QuantState::BeforeQuantifier;

    for tok in group {
        match (state, tok) {
            (QuantState::BeforeQuantifier, Token::Quantifier(_, _)) => {
                state = QuantState::CollectingClass;
            }
            (QuantState::CollectingClass, Token::Word(w)) => class_words.push(w.clone()),
            (QuantState::CollectingClass, Token::Exception) => {
                state = QuantState::CollectingExcept;
            }
            (QuantState::CollectingExcept, Token::Word(w)) => except_words.push(w.clone()),
            _ => {}
        }
    }

    let _ = quantifier; // captured by caller already
    let _ = exception_seen;

    let class = if class_words.is_empty() {
        "apps".to_string()
    } else {
        class_words.join(" ")
    };

    let except = if except_words.is_empty() {
        None
    } else {
        Some(except_words.join(" "))
    };

    Ok(TargetExpr::Quantified {
        quantifier,
        class,
        except,
    })
}

#[derive(Copy, Clone)]
enum QuantState {
    BeforeQuantifier,
    CollectingClass,
    CollectingExcept,
}

// ---- Inheritance ---------------------------------------------------------

fn resolve_verb_inheritance(
    own_verb: Option<VerbKind>,
    own_modifier: Option<ModifierKind>,
    inherited_verb: Option<VerbKind>,
    inherited_modifier: Option<ModifierKind>,
) -> (Option<VerbKind>, Option<ModifierKind>) {
    match (own_verb, own_modifier) {
        // Own verb explicit → don't inherit modifier (reset it).
        (Some(v), m) => (Some(v), m),
        // Modifier only → inherit verb, use own modifier.
        (None, Some(m)) => (inherited_verb, Some(m)),
        // Nothing → inherit both.
        (None, None) => (inherited_verb, inherited_modifier),
    }
}

// ---- Helpers -------------------------------------------------------------

fn verb_requires_target(v: VerbKind) -> bool {
    !matches!(v, VerbKind::Sleep | VerbKind::Restart | VerbKind::Shutdown)
}
fn is_no_target_mode_verb(v: VerbKind, mode_marker: bool) -> bool {
    mode_marker && matches!(v, VerbKind::ListVerb)
}
/// True if target tokens are not contiguous in the original token stream.
/// Used to mark interleaved-target parses as Corrected confidence.
fn is_interleaved(group: &[Token], target_indices: &[usize]) -> bool {
    if target_indices.len() < 2 {
        return false;
    }
    // Check that target indices are consecutive.
    for w in target_indices.windows(2) {
        if w[1] != w[0] + 1 {
            // Look at what's between them: if it's only verbs/modifiers/etc
            // (already extracted), that's an interleaved target.
            for between_idx in (w[0] + 1)..w[1] {
                match group.get(between_idx) {
                    Some(Token::Verb(_, _))
                    | Some(Token::Modifier(_))
                    | Some(Token::Number(_))
                    | Some(Token::Quantifier(_, _))
                    | Some(Token::Exception) => return true,
                    _ => {}
                }
            }
        }
    }
    false
}

fn compute_confidence(
    verb_was_corrected: bool,
    verb_was_present: bool,
    interleaved: bool,
) -> ParseConfidence {
    if !verb_was_present {
        ParseConfidence::Inferred
    } else if verb_was_corrected || interleaved {
        ParseConfidence::Corrected
    } else {
        ParseConfidence::Exact
    }
}

// ---- Tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::lexer::tokenize;

    fn parse_str(s: &str) -> Result<Command, ParseError> {
        parse(&tokenize(s))
    }

    fn list(s: &[&str]) -> TargetExpr {
        TargetExpr::List(s.iter().map(|x| x.to_string()).collect())
    }

    // ---- Existing behavior preserved -------------------------------------

    #[test]
    fn open_safari() {
        let cmd = parse_str("open safari").unwrap();
        let p = &cmd.phrases[0];
        assert_eq!(p.verb, Some(VerbKind::Open));
        assert_eq!(p.targets, list(&["safari"]));
        assert_eq!(p.parse_confidence, ParseConfidence::Exact);
    }

    #[test]
    fn open_multiword_app() {
        let cmd = parse_str("open visual studio code").unwrap();
        assert_eq!(cmd.phrases[0].targets, list(&["visual studio code"]));
    }

    #[test]
    fn turn_on_wifi_and_bluetooth_shared() {
        let cmd = parse_str("turn on wifi and bluetooth").unwrap();
        assert_eq!(cmd.phrases.len(), 2);
        assert_eq!(cmd.phrases[0].verb, Some(VerbKind::Turn));
        assert_eq!(cmd.phrases[0].modifier, Some(ModifierKind::On));
        assert_eq!(cmd.phrases[0].targets, list(&["wifi"]));
        // Phrase 2 inherits both verb and modifier.
        assert_eq!(cmd.phrases[1].verb, Some(VerbKind::Turn));
        assert_eq!(cmd.phrases[1].modifier, Some(ModifierKind::On));
        assert_eq!(cmd.phrases[1].targets, list(&["bluetooth"]));
    }

    #[test]
    fn turn_off_wifi_and_on_bluetooth_split_modifier() {
        let cmd = parse_str("turn off wifi and on bluetooth").unwrap();
        assert_eq!(cmd.phrases.len(), 2);
        assert_eq!(cmd.phrases[0].modifier, Some(ModifierKind::Off));
        assert_eq!(cmd.phrases[1].verb, Some(VerbKind::Turn));
        assert_eq!(cmd.phrases[1].modifier, Some(ModifierKind::On));
    }

    #[test]
    fn open_mail_and_close_safari_resets_modifier() {
        let cmd = parse_str("open mail and close safari").unwrap();
        assert_eq!(cmd.phrases.len(), 2);
        assert_eq!(cmd.phrases[0].verb, Some(VerbKind::Open));
        assert_eq!(cmd.phrases[1].verb, Some(VerbKind::Close));
        // Phrase 2 has its own verb → modifier is reset to None.
        assert_eq!(cmd.phrases[1].modifier, None);
    }

    // ---- Order independence ----------------------------------------------

    #[test]
    fn turn_off_bluetooth_normal_order() {
        let cmd = parse_str("turn off bluetooth").unwrap();
        let p = &cmd.phrases[0];
        assert_eq!(p.verb, Some(VerbKind::Turn));
        assert_eq!(p.modifier, Some(ModifierKind::Off));
        assert_eq!(p.targets, list(&["bluetooth"]));
    }

    #[test]
    fn turn_bluetooth_off_modifier_after_target() {
        let cmd = parse_str("turn bluetooth off").unwrap();
        let p = &cmd.phrases[0];
        assert_eq!(p.verb, Some(VerbKind::Turn));
        assert_eq!(p.modifier, Some(ModifierKind::Off));
        assert_eq!(p.targets, list(&["bluetooth"]));
    }

    #[test]
    fn bluetooth_off_no_verb() {
        let cmd = parse_str("bluetooth off").unwrap();
        let p = &cmd.phrases[0];
        assert_eq!(p.verb, None); // resolver will infer Turn
        assert_eq!(p.modifier, Some(ModifierKind::Off));
        assert_eq!(p.targets, list(&["bluetooth"]));
        assert_eq!(p.parse_confidence, ParseConfidence::Inferred);
    }

    #[test]
    fn off_bluetooth_modifier_first() {
        let cmd = parse_str("off bluetooth").unwrap();
        let p = &cmd.phrases[0];
        assert_eq!(p.verb, None);
        assert_eq!(p.modifier, Some(ModifierKind::Off));
        assert_eq!(p.targets, list(&["bluetooth"]));
    }

    // ---- Typo verbs (lowered confidence) ---------------------------------

    #[test]
    fn opne_typo_safari() {
        let cmd = parse_str("opne safari").unwrap();
        let p = &cmd.phrases[0];
        assert_eq!(p.verb, Some(VerbKind::Open));
        assert_eq!(p.parse_confidence, ParseConfidence::Corrected);
    }

    #[test]
    fn trun_on_wifi() {
        let cmd = parse_str("trun on wifi").unwrap();
        let p = &cmd.phrases[0];
        assert_eq!(p.verb, Some(VerbKind::Turn));
        assert_eq!(p.modifier, Some(ModifierKind::On));
        assert_eq!(p.parse_confidence, ParseConfidence::Corrected);
    }

    // ---- Numbers ---------------------------------------------------------

    #[test]
    fn set_volume_to_50() {
        let cmd = parse_str("set volume to 50").unwrap();
        let p = &cmd.phrases[0];
        assert_eq!(p.verb, Some(VerbKind::Set));
        assert_eq!(p.targets, list(&["volume"])); // "to" stripped
        assert_eq!(p.amount, Some(50));
    }

    #[test]
    fn increase_brightness_by_25() {
        let cmd = parse_str("increase brightness by 25%").unwrap();
        let p = &cmd.phrases[0];
        assert_eq!(p.verb, Some(VerbKind::Increase));
        assert_eq!(p.targets, list(&["brightness"]));
        assert_eq!(p.amount, Some(25));
    }

    #[test]
    fn implicit_set_via_number() {
        // "volume 50" — no verb, has number, has target.
        // Resolver will infer Set because target is a system setting.
        let cmd = parse_str("volume 50").unwrap();
        let p = &cmd.phrases[0];
        assert_eq!(p.verb, None);
        assert_eq!(p.amount, Some(50));
        assert_eq!(p.targets, list(&["volume"]));
        assert_eq!(p.parse_confidence, ParseConfidence::Inferred);
    }

    #[test]
    fn implicit_increase_via_up() {
        // "brightness up 20" — modifier Up, number 20, no verb.
        let cmd = parse_str("brightness up 20").unwrap();
        let p = &cmd.phrases[0];
        assert_eq!(p.verb, None);
        assert_eq!(p.modifier, Some(ModifierKind::Up));
        assert_eq!(p.amount, Some(20));
    }

    // ---- Quantified ------------------------------------------------------

    #[test]
    fn close_all_apps_except_calculator() {
        let cmd = parse_str("close all apps except calculator").unwrap();
        let p = &cmd.phrases[0];
        assert_eq!(p.verb, Some(VerbKind::Close));
        match &p.targets {
            TargetExpr::Quantified { quantifier, class, except } => {
                assert_eq!(*quantifier, QuantifierKind::All);
                assert_eq!(class, "apps");
                assert_eq!(except.as_deref(), Some("calculator"));
            }
            _ => panic!(),
        }
    }

    // ---- No-target verbs -------------------------------------------------

    #[test]
    fn sleep_alone() {
        let cmd = parse_str("sleep").unwrap();
        let p = &cmd.phrases[0];
        assert_eq!(p.verb, Some(VerbKind::Sleep));
        assert_eq!(p.targets, list(&[]));
    }

    #[test]
    fn shut_down() {
        let cmd = parse_str("shut down").unwrap();
        assert_eq!(cmd.phrases[0].verb, Some(VerbKind::Shutdown));
    }

    // ---- Errors ----------------------------------------------------------

    #[test]
    fn empty_errors() {
        assert!(matches!(parse_str(""), Err(ParseError::Empty)));
    }

    #[test]
    fn dangling_and_errors() {
        let r = parse_str("open safari and");
        assert!(matches!(r, Err(ParseError::DanglingConjunction)),
                "expected DanglingConjunction, got {:?}", r);
    }

    #[test]
    fn open_alone_errors() {
        // Has verb but no target.
        let r = parse_str("open");
        assert!(matches!(r, Err(ParseError::EmptyTarget)));
    }

    #[test]
    fn no_verb_no_modifier_errors() {
        // Just "wifi" — too ambiguous without modifier or number.
        // BUT "safari" alone is valid because resolver will infer Open.
        // The grammar doesn't know which is which; it just produces a
        // verb=None phrase with a target. Resolver decides.
        let cmd = parse_str("wifi").unwrap();
        assert_eq!(cmd.phrases[0].verb, None);
        assert_eq!(cmd.phrases[0].targets, list(&["wifi"]));
        // Resolver will throw ambiguity error here.
    }

    #[test]
    fn multiple_verbs_in_one_phrase_errors() {
        let r = parse_str("open close safari");
        assert!(matches!(r, Err(ParseError::MultipleVerbs)));
    }

    #[test]
    fn conflicting_modifiers_errors() {
        let r = parse_str("turn on off wifi");
        assert!(matches!(r, Err(ParseError::MultipleModifiers)));
    }

    #[test]
    fn semicolon_separates() {
        let cmd = parse_str("open safari; close mail").unwrap();
        assert_eq!(cmd.phrases.len(), 2);
        assert_eq!(cmd.phrases[0].verb, Some(VerbKind::Open));
        assert_eq!(cmd.phrases[1].verb, Some(VerbKind::Close));
    }

    // ---- Polite / filler -------------------------------------------------

    #[test]
    fn polite_command() {
        let cmd = parse_str("could you please open vs code").unwrap();
        assert_eq!(cmd.phrases[0].verb, Some(VerbKind::Open));
        assert_eq!(cmd.phrases[0].targets, list(&["vs code"]));
    }

    // ---- Open-without-verb (apps) ---------------------------------------

    #[test]
    fn safari_alone_no_verb() {
        // Resolver will infer Open because target is an app.
        let cmd = parse_str("safari").unwrap();
        let p = &cmd.phrases[0];
        assert_eq!(p.verb, None);
        assert_eq!(p.targets, list(&["safari"]));
    }
}