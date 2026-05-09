//! Intent types — the contract between the parser and the executor.
//!
//! Keep this module dependency-free so both sides can import it without
//! pulling in parser/executor internals.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Increase,
    Decrease,
    Set,        // explicit value, e.g. "set volume to 50"
    Toggle,
    TurnOn,
    TurnOff,
    Open,
    Close,
    CloseAll,
    Shutdown,
    Sleep,
    Restart,
    Unknown,
    // Add to whatever Intent enum / Action enum you have:
    ModeSave, 
    ModeSwitch, 
    ModeList,
    ModeDelete, 
    ModeExit,
    SummaryDebug,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Target {
    Volume,
    ScreenBrightness,
    KeyboardBrightness,
    WiFi,
    Bluetooth,
    DarkMode,         // <-- ADD
    DoNotDisturb, 
    App(String),
    Mode(String),
    System,
    None,
}

/// How sure are we about this parse?
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub enum Confidence {
    Low,    // best-guess; ask the user to confirm before destructive actions
    Medium,
    High,   // unambiguous match
}

#[derive(Debug, Clone)]
pub struct Intent {
    pub action: Action,
    pub target: Target,
    /// Optional magnitude: percent for volume/brightness, absolute for `Set`.
    /// `None` means "use the default step".
    pub amount: Option<i32>,
    pub confidence: Confidence,
    /// The original input, preserved for logging and error messages.
    pub raw: String,
}

impl Intent {
    pub fn unknown(raw: impl Into<String>) -> Self {
        Self {
            action: Action::Unknown,
            target: Target::None,
            amount: None,
            confidence: Confidence::Low,
            raw: raw.into(),
        }
    }
    pub fn confidence_score(&self) -> f32 {
        match self.confidence {
            Confidence::High   => 1.0,
            Confidence::Medium => 0.6,
            Confidence::Low    => 0.3,
        }
    }
}

 

impl fmt::Display for Intent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} {:?}", self.action, self.target)?;
        if let Some(n) = self.amount {
            write!(f, " ({}%)", n)?;
        }
        Ok(())
    }
}