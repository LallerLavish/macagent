// src/engines/capabilities/actions.rs
//
// Typed action splits. The previous monolithic Action enum allowed nonsense
// like (TurnOn, Volume) at compile time. These three types make such combos
// impossible — the type system enforces compatibility.
//
// Mapping from the existing Action enum (in intent.rs) is done by the
// resolver, which also knows the target type. The mapping table:
//
//   Old Action      Target type        Becomes
//   ---------------------------------------------------------------
//   TurnOn          BinaryTarget       BinaryAction::TurnOn
//   TurnOff         BinaryTarget       BinaryAction::TurnOff
//   Toggle          BinaryTarget       BinaryAction::Toggle
//   Set(N)          AnalogTarget       AnalogAction::Set(N)
//   Increase(N)     AnalogTarget       AnalogAction::Adjust(+N)
//   Decrease(N)     AnalogTarget       AnalogAction::Adjust(-N)
//   Sleep           N/A                TriggerAction::Sleep
//   Restart         N/A                TriggerAction::Restart
//   Shutdown        N/A                TriggerAction::Shutdown
//
// The resolver decides which split to use based on what the target IS — not
// what the user typed. So "set wifi to 50" produces an UnsupportedCombo
// error at the resolver level, not a runtime crash inside Wi-Fi capability.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BinaryAction {
    TurnOn,
    TurnOff,
    /// Flip current state. Capability decides whether to query+set or use
    /// an atomic toggle if supported.
    Toggle,
    /// Read current on/off state without changing it.
    Query,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalogAction {
    /// Set absolute value within the capability's range.
    Set(i32),
    /// Adjust by delta. Positive = increase, negative = decrease.
    Adjust(i32),
    /// Read current value without changing it.
    Query,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TriggerAction {
    Sleep,
    Restart,
    Shutdown,
    Lock,
}

impl BinaryAction {
    pub fn name(&self) -> &'static str {
        match self {
            BinaryAction::TurnOn  => "turn_on",
            BinaryAction::TurnOff => "turn_off",
            BinaryAction::Toggle  => "toggle",
            BinaryAction::Query   => "query",
        }
    }
}

impl AnalogAction {
    pub fn name(&self) -> &'static str {
        match self {
            AnalogAction::Set(_)    => "set",
            AnalogAction::Adjust(_) => "adjust",
            AnalogAction::Query     => "query",
        }
    }
}

impl TriggerAction {
    pub fn name(&self) -> &'static str {
        match self {
            TriggerAction::Sleep    => "sleep",
            TriggerAction::Restart  => "restart",
            TriggerAction::Shutdown => "shutdown",
            TriggerAction::Lock     => "lock",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_unique_per_type() {
        // Catches accidental name collisions if someone adds a variant later.
        let bin = [
            BinaryAction::TurnOn.name(),
            BinaryAction::TurnOff.name(),
            BinaryAction::Toggle.name(),
            BinaryAction::Query.name(),
        ];
        let mut seen = std::collections::HashSet::new();
        for n in bin {
            assert!(seen.insert(n), "duplicate name: {}", n);
        }
    }

    #[test]
    fn analog_set_carries_value() {
        let a = AnalogAction::Set(50);
        match a {
            AnalogAction::Set(v) => assert_eq!(v, 50),
            _ => panic!(),
        }
    }
}