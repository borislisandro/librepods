//! What each AirPods stem gesture does: left to the AirPods, or remapped to
//! an action LibrePods carries out on this computer.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StemGesture {
    Single,
    Double,
    Triple,
    Long,
}

impl StemGesture {
    pub const ALL: [StemGesture; 4] = [Self::Single, Self::Double, Self::Triple, Self::Long];

    pub fn label(self) -> &'static str {
        match self {
            Self::Single => "Single press",
            Self::Double => "Double press",
            Self::Triple => "Triple press",
            Self::Long => "Press and hold",
        }
    }

    /// What the AirPods do with this gesture when it isn't remapped.
    pub fn native_action(self) -> &'static str {
        match self {
            Self::Single => "play or pause",
            Self::Double => "next track",
            Self::Triple => "previous track",
            Self::Long => "switch listening mode",
        }
    }

    /// This gesture's bit in the StemConfig control command, which tells the
    /// AirPods to report the gesture to us instead of acting on it. (These
    /// differ from the StemPressType event values, 0x05-0x08.)
    fn config_bit(self) -> u8 {
        match self {
            Self::Single => 0x01,
            Self::Double => 0x02,
            Self::Triple => 0x04,
            Self::Long => 0x08,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum StemAction {
    /// Not remapped: the AirPods handle the gesture themselves.
    #[default]
    Default,
    PlayPause,
    NextTrack,
    PreviousTrack,
    CycleListeningMode,
    ToggleConversationAwareness,
    /// Press a keyboard key or shortcut, e.g. "PageDown" or "Ctrl+Shift+KeyM"
    /// (see keypress::KeyCombo).
    Key(String),
}

impl StemAction {
    /// The fixed actions, in the order the window lists them.
    pub const BUILT_IN: [StemAction; 6] = [
        Self::Default,
        Self::PlayPause,
        Self::NextTrack,
        Self::PreviousTrack,
        Self::CycleListeningMode,
        Self::ToggleConversationAwareness,
    ];
}

impl std::fmt::Display for StemAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Default => write!(f, "AirPods default"),
            Self::PlayPause => write!(f, "Play / pause"),
            Self::NextTrack => write!(f, "Next track"),
            Self::PreviousTrack => write!(f, "Previous track"),
            Self::CycleListeningMode => write!(f, "Switch listening mode"),
            Self::ToggleConversationAwareness => write!(f, "Conversation awareness on/off"),
            Self::Key(combo) => match crate::keypress::KeyCombo::parse(combo) {
                Some(combo) => write!(f, "Key: {}", combo.label()),
                None => write!(f, "Key: {combo} (unknown)"),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StemActions {
    pub single: StemAction,
    pub double: StemAction,
    pub triple: StemAction,
    pub long: StemAction,
}

impl StemActions {
    pub fn get(&self, gesture: StemGesture) -> &StemAction {
        match gesture {
            StemGesture::Single => &self.single,
            StemGesture::Double => &self.double,
            StemGesture::Triple => &self.triple,
            StemGesture::Long => &self.long,
        }
    }

    pub fn set(&mut self, gesture: StemGesture, action: StemAction) {
        match gesture {
            StemGesture::Single => self.single = action,
            StemGesture::Double => self.double = action,
            StemGesture::Triple => self.triple = action,
            StemGesture::Long => self.long = action,
        }
    }

    /// StemConfig value: the gestures the AirPods should hand over to us.
    pub fn config_mask(&self) -> u8 {
        StemGesture::ALL
            .into_iter()
            .filter(|&g| *self.get(g) != StemAction::Default)
            .fold(0, |mask, g| mask | g.config_bit())
    }

    pub fn uses_keys(&self) -> bool {
        StemGesture::ALL
            .into_iter()
            .any(|g| matches!(self.get(g), StemAction::Key(_)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_covers_only_remapped_gestures() {
        let mut actions = StemActions::default();
        assert_eq!(actions.config_mask(), 0);
        actions.set(StemGesture::Triple, StemAction::Key("PageDown".into()));
        assert_eq!(actions.config_mask(), 0x04);
        actions.set(StemGesture::Long, StemAction::PlayPause);
        assert_eq!(actions.config_mask(), 0x0c);
        assert!(actions.uses_keys());
    }

    #[test]
    fn settings_round_trip() {
        let mut actions = StemActions::default();
        actions.set(StemGesture::Triple, StemAction::Key("Ctrl+KeyM".into()));
        let json = serde_json::to_string(&actions).unwrap();
        assert_eq!(serde_json::from_str::<StemActions>(&json).unwrap(), actions);
    }

    #[test]
    fn labels() {
        assert_eq!(
            StemAction::Key("Ctrl+Shift+PageDown".into()).to_string(),
            "Key: Ctrl + Shift + Page Down"
        );
    }
}
