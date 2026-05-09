// src/pipeline/resolver.rs
//
// Resolver v2: grammar AST → list of executable Intents.
//
// Major change from v1: `phrase.verb` is now `Option<VerbKind>`. When None,
// the resolver infers the action based on the resolved target type:
//
//   - App target with no verb           → Open
//   - System setting with on/off mod    → Turn (TurnOn / TurnOff)
//   - System setting with up/down mod   → Increase / Decrease
//   - System setting with number, no mod→ Set
//   - System setting alone (no mod, no number) → ambiguity error
//   - App target with on/off            → unsupported (apps don't have on/off)
//
// Confidence handling: parse_confidence from grammar is combined with
// match score from registry to produce the final Intent confidence.

use std::sync::Arc;

use crate::engines::actions::intent::{Action, Confidence, Intent, Target};
use crate::registry::{AppRegistry, ResolutionResult};

use super::grammar::{ActionPhrase, Command, ParseConfidence, TargetExpr};
use super::lexer::{ModifierKind, QuantifierKind, VerbKind};

// ---- Errors --------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum ResolveError {
    AppNotFound(String),
    AmbiguousApp {
        query: String,
        candidates: Vec<String>,
    },
    UnknownSetting(String),
    UnsupportedCombo { action: String, target: String },
    UnknownClass(String),
    EmptyTarget,
    /// Verb=None and target is a system setting with no modifier/number to
    /// disambiguate. e.g. "wifi" alone — turn on or off?
    AmbiguousVerb(String),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::AppNotFound(q) => write!(f, "no app found with name '{}'", q),
            ResolveError::AmbiguousApp { query, candidates } => write!(
                f,
                "ambiguous app name '{}', candidates: {}",
                query,
                candidates.join(", ")
            ),
            ResolveError::UnknownSetting(s) => write!(f, "unknown setting: '{}'", s),
            ResolveError::UnsupportedCombo { action, target } => {
                write!(f, "cannot {} {}", action, target)
            }
            ResolveError::UnknownClass(c) => write!(f, "unknown target class: '{}'", c),
            ResolveError::EmptyTarget => write!(f, "no target given"),
            ResolveError::AmbiguousVerb(t) => write!(
                f,
                "ambiguous: did you mean to turn '{}' on or off?",
                t
            ),
        }
    }
}

impl std::error::Error for ResolveError {}

impl ResolveError {
    pub fn kind_str(&self) -> &'static str {
        match self {
            ResolveError::AppNotFound(_)        => "AppNotFound",
            ResolveError::AmbiguousApp { .. }   => "AmbiguousApp",
            ResolveError::UnknownSetting(_)     => "UnknownSetting",
            ResolveError::UnsupportedCombo {..} => "UnsupportedCombo",
            ResolveError::UnknownClass(_)       => "UnknownClass",
            ResolveError::EmptyTarget           => "EmptyTarget",
            ResolveError::AmbiguousVerb(_)      => "AmbiguousVerb",
        }
    }
}

#[derive(Debug)]
pub enum ResolvedTarget {
    Intent(Intent),
    Error(ResolveError),
}

// ---- Entry point ---------------------------------------------------------

pub fn resolve_command(
    cmd: &Command,
    apps: &Arc<AppRegistry>,
) -> Vec<ResolvedTarget> {
    let mut out = Vec::new();
    for phrase in &cmd.phrases {
        resolve_phrase(phrase, apps, &mut out);
    }
    out
}

// ---- Phrase resolution ---------------------------------------------------

fn resolve_phrase(
    phrase: &ActionPhrase,
    apps: &Arc<AppRegistry>,
    out: &mut Vec<ResolvedTarget>,
) {
    if phrase.is_mode_command {
        resolve_mode_phrase(phrase, out);
        return;
    }
    // Verb-only phrases (Sleep / Restart / Shutdown) — no target work needed.
    if let Some(v) = phrase.verb {
        if !verb_needs_target(v) {
            let action = map_simple_verb(v);
            out.push(ResolvedTarget::Intent(Intent {
                action,
                target: Target::System,
                amount: phrase.amount,
                confidence: parse_to_intent_confidence(phrase.parse_confidence),
                raw: format!("{:?}", v),
            }));
            return;
        }
    }

    // Quantified targets.
    if let TargetExpr::Quantified { quantifier, class, except } = &phrase.targets {
        let action = phrase
            .verb
            .map(map_simple_verb)
            .unwrap_or(Action::Close); // sensible default for "all apps except X"
        resolve_quantified(action, *quantifier, class, except.as_deref(), apps, phrase.amount, out);
        return;
    }

    // Target list path.
    let TargetExpr::List(targets) = &phrase.targets else {
        unreachable!("Quantified handled above");
    };

    if targets.is_empty() {
        out.push(ResolvedTarget::Error(ResolveError::EmptyTarget));
        return;
    }

    for target_str in targets {
        resolve_target_string(
            phrase.verb,
            phrase.modifier,
            target_str,
            phrase.amount,
            phrase.parse_confidence,
            apps,
            out,
        );
    }
}

// ---- Verb mapping --------------------------------------------------------

/// Map a simple verb (where modifier doesn't change the action) to Action.
/// Used for Open/Close/Sleep/Restart/Shutdown/Toggle/Set/Increase/Decrease.
fn map_simple_verb(v: VerbKind) -> Action {
    match v {
        VerbKind::Open     => Action::Open,
        VerbKind::Close    => Action::Close,
        VerbKind::Toggle   => Action::Toggle,
        VerbKind::Set      => Action::Set,
        VerbKind::Increase => Action::Increase,
        VerbKind::Decrease => Action::Decrease,
        VerbKind::Enable   => Action::TurnOn,
        VerbKind::Disable  => Action::TurnOff,
        VerbKind::Sleep    => Action::Sleep,
        VerbKind::Restart  => Action::Restart,
        VerbKind::Shutdown => Action::Shutdown,
        // Turn requires a modifier — caller must use map_verb_with_modifier.
        VerbKind::Turn     => Action::Toggle, // fallback if no modifier
       VerbKind::Save | VerbKind::Debug | VerbKind::Switch | VerbKind::ListVerb 
            | VerbKind::Delete | VerbKind::Leave => {
            unreachable!("mode verb reached map_simple_verb — routing bug")
        }
    }
}

/// Map (verb, modifier) → Action. Returns None for nonsensical combos.
fn map_verb_with_modifier(v: VerbKind, m: Option<ModifierKind>) -> Option<Action> {
    use ModifierKind::*;
    use VerbKind::*;

    Some(match (v, m) {
        (Turn, Some(On))    => Action::TurnOn,
        (Turn, Some(Off))   => Action::TurnOff,
        (Turn, Some(Up))    => Action::Increase,
        (Turn, Some(Down))  => Action::Decrease,
        (Turn, None)        => return None, // "turn wifi" alone makes no sense
        (Enable, _)         => Action::TurnOn,
        (Disable, _)        => Action::TurnOff,
        (Increase, _) | (_, Some(Up))   => Action::Increase,
        (Decrease, _) | (_, Some(Down)) => Action::Decrease,
        // Open/Close don't take modifiers — modifier is ignored.
        (Open, _)           => Action::Open,
        (Close, _)          => Action::Close,
        (Toggle, _)         => Action::Toggle,
        (Set, _)            => Action::Set,
        (Sleep, _)          => Action::Sleep,
        (Restart, _)        => Action::Restart,
        (Shutdown, _)       => Action::Shutdown,
        (VerbKind::Save, _) => Action::ModeSave,
        (VerbKind::Switch, _) => Action::ModeSwitch,
        (VerbKind::ListVerb, _) => Action::ModeList,
        (VerbKind::Delete, _) => Action::ModeDelete,
        (VerbKind::Save | VerbKind::Debug | VerbKind::Switch | VerbKind::ListVerb 
            | VerbKind::Delete | VerbKind::Leave, _) => {
            unreachable!("mode verb reached map_verb_with_modifier — routing bug")
        }
    })
}

fn verb_needs_target(v: VerbKind) -> bool {
    !matches!(v, VerbKind::Sleep | VerbKind::Restart | VerbKind::Shutdown)
}

// ---- Target type detection ----------------------------------------------

/// Map a target string to a system-setting Target if recognized.
fn resolve_system_target(s: &str) -> Option<Target> {
    let s = s.trim().to_lowercase();
    Some(match s.as_str() {
        "wifi" | "wi-fi" | "wireless"            => Target::WiFi,
        "bluetooth" | "bt"                       => Target::Bluetooth,
        "volume" | "sound" | "audio"             => Target::Volume,
        "brightness" | "screen brightness" | "display" => Target::ScreenBrightness,
        "keyboard brightness" | "keyboard backlight"   => Target::KeyboardBrightness,
        "dark mode" | "darkmode" | "dark"        => Target::DarkMode,        // ADD
        "do not disturb" | "dnd" | "focus" | "focus mode" => Target::DoNotDisturb,  // ADD
        _ => return None,
    })
}

// ---- Single target resolution -------------------------------------------

fn resolve_target_string(
    verb: Option<VerbKind>,
    modifier: Option<ModifierKind>,
    raw: &str,
    amount: Option<i32>,
    parse_conf: ParseConfidence,
    apps: &Arc<AppRegistry>,
    out: &mut Vec<ResolvedTarget>,
) {
    // First: figure out what kind of target this is.
    let sys_target = resolve_system_target(raw);

    if let Some(target) = sys_target {
        // System setting path. Determine action.
        let action = match (verb, modifier, amount) {
            // Explicit verb → trust it.
            (Some(v), m, _) => match map_verb_with_modifier(v, m) {
                Some(a) => a,
                None => {
                    out.push(ResolvedTarget::Error(ResolveError::UnsupportedCombo {
                        action: format!("{:?}", v),
                        target: raw.to_string(),
                    }));
                    return;
                }
            },
            // No verb — infer from modifier/amount.
            (None, Some(ModifierKind::On),   _) => Action::TurnOn,
            (None, Some(ModifierKind::Off),  _) => Action::TurnOff,
            (None, Some(ModifierKind::Up),   _) => Action::Increase,
            (None, Some(ModifierKind::Down), _) => Action::Decrease,
            (None, None, Some(_))               => Action::Set,
            (None, None, None) => {
                // "wifi" alone — ambiguous.
                out.push(ResolvedTarget::Error(ResolveError::AmbiguousVerb(
                    raw.to_string(),
                )));
                return;
            }
        };

        out.push(ResolvedTarget::Intent(Intent {
            action,
            target,
            amount,
            confidence: parse_to_intent_confidence(parse_conf),
            raw: raw.to_string(),
        }));
        return;
    }

    // Not a system setting → must be an app. Determine action.
    let app_action = match verb {
        Some(VerbKind::Open) => Action::Open,
        Some(VerbKind::Close) => Action::Close,
        Some(other) => {
            // Verb that doesn't apply to apps (Turn, Set, etc.) on a non-system target.
            out.push(ResolvedTarget::Error(ResolveError::UnsupportedCombo {
                action: format!("{:?}", other),
                target: raw.to_string(),
            }));
            return;
        }
        // No verb on an app target → infer Open.
        None => Action::Open,
    };

    // App resolution via greedy registry tokenization.
    let resolved_apps = greedy_resolve_apps(raw, apps);
    if resolved_apps.is_empty() {
        out.push(ResolvedTarget::Error(ResolveError::AppNotFound(
            raw.to_string(),
        )));
        return;
    }

    for app_outcome in resolved_apps {
        match app_outcome {
            AppResolution::Found { canonical, score } => {
                out.push(ResolvedTarget::Intent(Intent {
                    action: app_action,
                    target: Target::App(canonical),
                    amount,
                    confidence: combine_confidence(parse_conf, score),
                    raw: raw.to_string(),
                }));
            }
            AppResolution::Ambiguous { query, candidates } => {
                out.push(ResolvedTarget::Error(ResolveError::AmbiguousApp {
                    query,
                    candidates,
                }));
            }
            AppResolution::NotFound(q) => {
                out.push(ResolvedTarget::Error(ResolveError::AppNotFound(q)));
            }
        }
    }
}

// ---- Confidence combination ---------------------------------------------

fn parse_to_intent_confidence(p: ParseConfidence) -> Confidence {
    match p {
        ParseConfidence::Exact     => Confidence::High,
        ParseConfidence::Corrected => Confidence::Medium,
        ParseConfidence::Inferred  => Confidence::Medium,
    }
}

fn combine_confidence(parse_conf: ParseConfidence, match_score: f32) -> Confidence {
    let parse_floor = match parse_conf {
        ParseConfidence::Exact     => Confidence::High,
        ParseConfidence::Corrected => Confidence::Medium,
        ParseConfidence::Inferred  => Confidence::Medium,
    };
    let match_level = if match_score >= 0.85 {
        Confidence::High
    } else if match_score >= 0.70 {
        Confidence::Medium
    } else {
        Confidence::Low
    };
    // Take the lower of the two.
    if (parse_floor as u8) < (match_level as u8) {
        parse_floor
    } else {
        match_level
    }
}

// ---- Greedy app tokenization (unchanged from earlier fix) ---------------

#[derive(Debug)]
enum AppResolution {
    Found { canonical: String, score: f32 },
    Ambiguous { query: String, candidates: Vec<String> },
    NotFound(String),
}

fn greedy_resolve_apps(raw: &str, apps: &Arc<AppRegistry>) -> Vec<AppResolution> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    match apps.resolve(trimmed) {
        ResolutionResult::Confident { canonical, score } => {
            if match_covers_input(trimmed, &canonical) {
                return vec![AppResolution::Found { canonical, score }];
            }
        }
        ResolutionResult::Ambiguous { candidates } => {
            if !trimmed.contains(char::is_whitespace) {
                return vec![AppResolution::Ambiguous {
                    query: trimmed.to_string(),
                    candidates: candidates.into_iter().map(|(n, _)| n).collect(),
                }];
            }
        }
        ResolutionResult::NotFound => {}
    }

    let words: Vec<&str> = trimmed.split_whitespace().collect();
    if words.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let mut matched = false;
        for j in (i + 1..=words.len()).rev() {
            let candidate = words[i..j].join(" ");
            if let ResolutionResult::Confident { canonical, score } = apps.resolve(&candidate) {
                if match_covers_input(&candidate, &canonical) {
                    out.push(AppResolution::Found { canonical, score });
                    i = j;
                    matched = true;
                    break;
                }
            }
        }
        if !matched {
            out.push(AppResolution::NotFound(words[i].to_string()));
            i += 1;
        }
    }
    out
}

fn match_covers_input(input: &str, canonical: &str) -> bool {
    let in_compressed: String = input
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect();
    let can_compressed: String = canonical
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect();

    if !input.contains(char::is_whitespace) {
        return true;
    }
    can_compressed.len() >= in_compressed.len()
}

// ---- Quantified resolution ----------------------------------------------

fn resolve_quantified(
    action: Action,
    _quantifier: QuantifierKind,
    class: &str,
    except: Option<&str>,
    apps: &Arc<AppRegistry>,
    amount: Option<i32>,
    out: &mut Vec<ResolvedTarget>,
) {
    let class_lower = class.trim().to_lowercase();

    if !matches!(class_lower.as_str(), "apps" | "applications" | "app" | "application") {
        out.push(ResolvedTarget::Error(ResolveError::UnknownClass(
            class.to_string(),
        )));
        return;
    }

    if !matches!(action, Action::Close | Action::CloseAll | Action::Open) {
        out.push(ResolvedTarget::Error(ResolveError::UnsupportedCombo {
            action: format!("{:?}", action),
            target: format!("all {}", class),
        }));
        return;
    }

    let except_canonical: Option<String> = except.and_then(|e| match apps.resolve(e) {
        ResolutionResult::Confident { canonical, .. } => Some(canonical),
        _ => None,
    });

    for entry in apps.all() {
        if let Some(skip) = &except_canonical {
            if &entry.display_name == skip {
                continue;
            }
        }
        out.push(ResolvedTarget::Intent(Intent {
            action,
            target: Target::App(entry.display_name.clone()),
            amount,
            confidence: Confidence::High,
            raw: format!("all {} except {:?}", class, except),
        }));
    }
}

// ---- Mode command resolution --------------------------------------------

fn resolve_mode_phrase(phrase: &ActionPhrase, out: &mut Vec<ResolvedTarget>) {
    let action = match phrase.verb {
        Some(VerbKind::Save)     => Action::ModeSave,
        Some(VerbKind::Switch)   => Action::ModeSwitch,
        Some(VerbKind::ListVerb) => Action::ModeList,
        Some(VerbKind::Delete)   => Action::ModeDelete,
        Some(VerbKind::Leave)    => Action::ModeExit,
        Some(VerbKind::Debug) => Action::SummaryDebug,
        // Verb-inferred case: "<name> mode" with no explicit verb → Switch.
        // (e.g. "work mode" or "work mode on")
        None => Action::ModeSwitch,
        Some(other) => {
            out.push(ResolvedTarget::Error(ResolveError::UnsupportedCombo {
                action: format!("{:?}", other),
                target: "mode".to_string(),
            }));
            return;
        }
    };

    // Extract mode name from target. ModeList takes no name.
    let name = match &phrase.targets {
        TargetExpr::List(words) => words.join(" ").trim().to_string(),
        TargetExpr::Quantified { .. } => {
            // "all modes" or similar — not supported
            out.push(ResolvedTarget::Error(ResolveError::UnsupportedCombo {
                action: format!("{:?}", action),
                target: "quantified mode expression".to_string(),
            }));
            return;
        }
    };

    let target = match action {
        Action::ModeList | Action::SummaryDebug => {
            // "list modes" — no name needed. Reject if user gave one.
            if !name.is_empty() {
                out.push(ResolvedTarget::Error(ResolveError::UnsupportedCombo {
                    action: "ModeList".to_string(),
                    target: format!("specific mode '{}'", name),
                }));
                return;
            }
            Target::None
        }
        Action::ModeSave | Action::ModeSwitch | Action::ModeDelete |Action::ModeExit => {
            if name.is_empty() {
                out.push(ResolvedTarget::Error(ResolveError::EmptyTarget));
                return;
            }
            // Strip filler tokens that may have leaked through ("current",
            // "state", "as", "to" — these aren't fillers in the lexer's
            // global table because they're meaningful elsewhere).
            let cleaned = clean_mode_name(&name);
            if cleaned.is_empty() {
                out.push(ResolvedTarget::Error(ResolveError::EmptyTarget));
                return;
            }
            Target::Mode(cleaned)
        }
        _ => unreachable!("non-mode action reached resolve_mode_phrase"),
    };

    out.push(ResolvedTarget::Intent(Intent {
        action,
        target,
        amount: None,
        confidence: parse_to_intent_confidence(phrase.parse_confidence),
        raw: format_phrase_raw(phrase),
    }));
}

/// Strip mode-context filler words from a captured target string.
/// "current state as work" → "work"
/// "to work"               → "work"
fn clean_mode_name(s: &str) -> String {
    const MODE_FILLERS: &[&str] = &["current", "state", "as", "to"];
    s.split_whitespace()
        .filter(|w| !MODE_FILLERS.contains(&w.to_lowercase().as_str()))
        .collect::<Vec<_>>()
        .join("_")
}

fn format_phrase_raw(phrase: &ActionPhrase) -> String {
    match &phrase.targets {
        TargetExpr::List(words) => words.join(" "),
        TargetExpr::Quantified { class, .. } => class.clone(),
    }
}



#[cfg(test)]
mod tests {
    use super::*;
    use super::super::grammar::parse;
    use super::super::lexer::tokenize;

    fn resolve_str(s: &str, apps: &Arc<AppRegistry>) -> Vec<ResolvedTarget> {
        let cmd = parse(&tokenize(s)).expect("should parse");
        resolve_command(&cmd, apps)
    }

    fn fresh_registry() -> Arc<AppRegistry> {
        Arc::new(AppRegistry::scan_now().expect("scan should succeed"))
    }

    #[test]
    fn turn_on_wifi() {
        let r = resolve_str("turn on wifi", &fresh_registry());
        assert_eq!(r.len(), 1);
        match &r[0] {
            ResolvedTarget::Intent(i) => {
                assert_eq!(i.action, Action::TurnOn);
                assert_eq!(i.target, Target::WiFi);
            }
            o => panic!("{:?}", o),
        }
    }

    #[test]
    fn bluetooth_off_no_verb() {
        let r = resolve_str("bluetooth off", &fresh_registry());
        assert_eq!(r.len(), 1);
        match &r[0] {
            ResolvedTarget::Intent(i) => {
                assert_eq!(i.action, Action::TurnOff);
                assert_eq!(i.target, Target::Bluetooth);
            }
            o => panic!("{:?}", o),
        }
    }

    #[test]
    fn off_bluetooth_modifier_first() {
        let r = resolve_str("off bluetooth", &fresh_registry());
        assert_eq!(r.len(), 1);
        match &r[0] {
            ResolvedTarget::Intent(i) => {
                assert_eq!(i.action, Action::TurnOff);
                assert_eq!(i.target, Target::Bluetooth);
            }
            o => panic!("{:?}", o),
        }
    }

    #[test]
    fn turn_bluetooth_off() {
        let r = resolve_str("turn bluetooth off", &fresh_registry());
        match &r[0] {
            ResolvedTarget::Intent(i) => {
                assert_eq!(i.action, Action::TurnOff);
                assert_eq!(i.target, Target::Bluetooth);
            }
            o => panic!("{:?}", o),
        }
    }

    #[test]
    fn wifi_alone_is_ambiguous() {
        let r = resolve_str("wifi", &fresh_registry());
        assert_eq!(r.len(), 1);
        matches!(&r[0], ResolvedTarget::Error(ResolveError::AmbiguousVerb(_)));
    }

    #[test]
    fn volume_50_implicit_set() {
        let r = resolve_str("volume 50", &fresh_registry());
        match &r[0] {
            ResolvedTarget::Intent(i) => {
                assert_eq!(i.action, Action::Set);
                assert_eq!(i.target, Target::Volume);
                assert_eq!(i.amount, Some(50));
            }
            o => panic!("{:?}", o),
        }
    }

    #[test]
    fn brightness_up_20_implicit_increase() {
        let r = resolve_str("brightness up 20", &fresh_registry());
        match &r[0] {
            ResolvedTarget::Intent(i) => {
                assert_eq!(i.action, Action::Increase);
                assert_eq!(i.target, Target::ScreenBrightness);
                assert_eq!(i.amount, Some(20));
            }
            o => panic!("{:?}", o),
        }
    }

    #[test]
    fn turn_off_wifi_and_on_bluetooth() {
        let r = resolve_str("turn off wifi and on bluetooth", &fresh_registry());
        assert_eq!(r.len(), 2);
        match &r[0] {
            ResolvedTarget::Intent(i) => {
                assert_eq!(i.action, Action::TurnOff);
                assert_eq!(i.target, Target::WiFi);
            }
            _ => panic!(),
        }
        match &r[1] {
            ResolvedTarget::Intent(i) => {
                assert_eq!(i.action, Action::TurnOn);
                assert_eq!(i.target, Target::Bluetooth);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn opne_typo_still_resolves() {
        let r = resolve_str("opne safari", &fresh_registry());
        assert_eq!(r.len(), 1);
        match &r[0] {
            ResolvedTarget::Intent(i) => assert_eq!(i.action, Action::Open),
            o => panic!("{:?}", o),
        }
    }

    #[test]
    fn sleep_works() {
        let r = resolve_str("sleep", &fresh_registry());
        match &r[0] {
            ResolvedTarget::Intent(i) => {
                assert_eq!(i.action, Action::Sleep);
                assert_eq!(i.target, Target::System);
            }
            o => panic!("{:?}", o),
        }
    }


// ---- Tests ---------------------------------------------------------------
#[test]
    fn save_work_mode() {
        let r = resolve_str("save work mode", &fresh_registry());
        assert_eq!(r.len(), 1);
        match &r[0] {
            ResolvedTarget::Intent(i) => {
                assert_eq!(i.action, Action::ModeSave);
                assert_eq!(i.target, Target::Mode("work".to_string()));
            }
            o => panic!("{:?}", o),
        }
    }

    #[test]
    fn save_current_state_as_work_mode() {
        let r = resolve_str("save current state as work mode", &fresh_registry());
        match &r[0] {
            ResolvedTarget::Intent(i) => {
                assert_eq!(i.action, Action::ModeSave);
                assert_eq!(i.target, Target::Mode("work".to_string()));
            }
            o => panic!("{:?}", o),
        }
    }

    #[test]
    fn switch_to_work_mode() {
        let r = resolve_str("switch to work mode", &fresh_registry());
        match &r[0] {
            ResolvedTarget::Intent(i) => {
                assert_eq!(i.action, Action::ModeSwitch);
                assert_eq!(i.target, Target::Mode("work".to_string()));
            }
            o => panic!("{:?}", o),
        }
    }

    #[test]
    fn work_mode_implicit_switch() {
        // No explicit verb — resolver infers Switch.
        let r = resolve_str("work mode", &fresh_registry());
        match &r[0] {
            ResolvedTarget::Intent(i) => {
                assert_eq!(i.action, Action::ModeSwitch);
                assert_eq!(i.target, Target::Mode("work".to_string()));
            }
            o => panic!("{:?}", o),
        }
    }

    #[test]
    fn list_modes() {
        let r = resolve_str("list modes", &fresh_registry());
        match &r[0] {
            ResolvedTarget::Intent(i) => {
                assert_eq!(i.action, Action::ModeList);
                assert_eq!(i.target, Target::None);
            }
            o => panic!("{:?}", o),
        }
    }

    #[test]
    fn delete_work_mode() {
        let r = resolve_str("delete work mode", &fresh_registry());
        match &r[0] {
            ResolvedTarget::Intent(i) => {
                assert_eq!(i.action, Action::ModeDelete);
                assert_eq!(i.target, Target::Mode("work".to_string()));
            }
            o => panic!("{:?}", o),
        }
    }
}
