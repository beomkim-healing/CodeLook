//! Configurable keyboard shortcuts (IntelliJ-style macOS defaults).
//! Chords are matched with `consume_key`, so a handled shortcut never leaks
//! into a focused text field. The map serializes to a single storage string
//! (`action=cmd+shift+F` per line); missing/unknown entries fall back to the
//! defaults, so new actions appear automatically after an update.

use eframe::egui::{Context, Key, Modifiers};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Action {
    FindInFile,
    FindInProject,
    GoToFile,
    GoToSymbol,
    GoToLine,
    RecentFiles,
    GoToDeclaration,
    Back,
    Forward,
    ToggleProject,
    ToggleStructure,
    ToggleCommits,
    CloseTab,
    PrevTab,
    NextTab,
    ZoomIn,
    ZoomOut,
}

/// Display / settings order.
pub const ACTIONS: &[Action] = &[
    Action::GoToFile,
    Action::GoToSymbol,
    Action::RecentFiles,
    Action::GoToLine,
    Action::FindInFile,
    Action::FindInProject,
    Action::GoToDeclaration,
    Action::Back,
    Action::Forward,
    Action::CloseTab,
    Action::PrevTab,
    Action::NextTab,
    Action::ToggleProject,
    Action::ToggleStructure,
    Action::ToggleCommits,
    Action::ZoomIn,
    Action::ZoomOut,
];

impl Action {
    pub fn label(&self) -> &'static str {
        match self {
            Action::FindInFile => "파일 내 검색",
            Action::FindInProject => "전체 검색 (Find in Files)",
            Action::GoToFile => "파일로 이동 (Go to File)",
            Action::GoToSymbol => "심볼로 이동 (Go to Symbol)",
            Action::GoToLine => "줄로 이동 (Go to Line)",
            Action::RecentFiles => "최근 파일",
            Action::GoToDeclaration => "정의로 이동 (Go to Declaration)",
            Action::Back => "뒤로 가기",
            Action::Forward => "앞으로 가기",
            Action::ToggleProject => "프로젝트 패널 토글",
            Action::ToggleStructure => "구조 패널 토글",
            Action::ToggleCommits => "커밋 패널 토글",
            Action::CloseTab => "탭 닫기",
            Action::PrevTab => "이전 탭",
            Action::NextTab => "다음 탭",
            Action::ZoomIn => "글자 크게",
            Action::ZoomOut => "글자 작게",
        }
    }

    fn storage_key(&self) -> &'static str {
        match self {
            Action::FindInFile => "find_in_file",
            Action::FindInProject => "find_in_project",
            Action::GoToFile => "go_to_file",
            Action::GoToSymbol => "go_to_symbol",
            Action::GoToLine => "go_to_line",
            Action::RecentFiles => "recent_files",
            Action::GoToDeclaration => "go_to_declaration",
            Action::Back => "back",
            Action::Forward => "forward",
            Action::ToggleProject => "toggle_project",
            Action::ToggleStructure => "toggle_structure",
            Action::ToggleCommits => "toggle_commits",
            Action::CloseTab => "close_tab",
            Action::PrevTab => "prev_tab",
            Action::NextTab => "next_tab",
            Action::ZoomIn => "zoom_in",
            Action::ZoomOut => "zoom_out",
        }
    }

    fn default_chord(&self) -> Chord {
        let cmd = Modifiers::COMMAND;
        let cmd_shift = Modifiers::COMMAND | Modifiers::SHIFT;
        let cmd_alt = Modifiers::COMMAND | Modifiers::ALT;
        match self {
            Action::FindInFile => Chord::new(cmd, Key::F),
            Action::FindInProject => Chord::new(cmd_shift, Key::F),
            Action::GoToFile => Chord::new(cmd_shift, Key::O),
            Action::GoToSymbol => Chord::new(cmd_alt, Key::O),
            Action::GoToLine => Chord::new(cmd, Key::L),
            Action::RecentFiles => Chord::new(cmd, Key::E),
            Action::GoToDeclaration => Chord::new(cmd, Key::B),
            Action::Back => Chord::new(cmd, Key::OpenBracket),
            Action::Forward => Chord::new(cmd, Key::CloseBracket),
            Action::ToggleProject => Chord::new(cmd, Key::Num1),
            Action::ToggleStructure => Chord::new(cmd, Key::Num7),
            Action::ToggleCommits => Chord::new(cmd, Key::Num9),
            Action::CloseTab => Chord::new(cmd, Key::W),
            Action::PrevTab => Chord::new(cmd_shift, Key::OpenBracket),
            Action::NextTab => Chord::new(cmd_shift, Key::CloseBracket),
            Action::ZoomIn => Chord::new(cmd, Key::Equals),
            Action::ZoomOut => Chord::new(cmd, Key::Minus),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub struct Chord {
    pub mods: Modifiers,
    pub key: Key,
}

impl Chord {
    pub fn new(mods: Modifiers, key: Key) -> Self {
        Self { mods, key }
    }

    /// Mac-style display text, e.g. "⇧⌘F".
    pub fn text(&self) -> String {
        let mut s = String::new();
        if self.mods.ctrl {
            s.push('⌃');
        }
        if self.mods.alt {
            s.push('⌥');
        }
        if self.mods.shift {
            s.push('⇧');
        }
        if self.mods.command || self.mods.mac_cmd {
            s.push('⌘');
        }
        s.push_str(self.key.symbol_or_name());
        s
    }

    fn serialize(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if self.mods.ctrl {
            parts.push("ctrl");
        }
        if self.mods.alt {
            parts.push("alt");
        }
        if self.mods.shift {
            parts.push("shift");
        }
        if self.mods.command || self.mods.mac_cmd {
            parts.push("cmd");
        }
        parts.push(self.key.name());
        parts.join("+")
    }

    fn parse(s: &str) -> Option<Self> {
        let mut mods = Modifiers::NONE;
        let mut key = None;
        for part in s.split('+') {
            match part {
                "ctrl" => mods.ctrl = true,
                "alt" => mods.alt = true,
                "shift" => mods.shift = true,
                "cmd" => mods.command = true,
                other => key = Key::from_name(other),
            }
        }
        key.map(|key| Self { mods, key })
    }
}

pub struct Keymap {
    map: std::collections::HashMap<Action, Chord>,
}

impl Keymap {
    pub fn default_map() -> Self {
        let mut map = std::collections::HashMap::new();
        for &a in ACTIONS {
            map.insert(a, a.default_chord());
        }
        Self { map }
    }

    pub fn get(&self, a: Action) -> Chord {
        self.map.get(&a).copied().unwrap_or_else(|| a.default_chord())
    }

    pub fn set(&mut self, a: Action, mut c: Chord) {
        // Normalize the mac-specific cmd flag so equality/conflict checks
        // compare logically identical chords.
        if c.mods.mac_cmd {
            c.mods.mac_cmd = false;
            c.mods.command = true;
        }
        self.map.insert(a, c);
    }

    /// Actions (other than `a`) already bound to `c`.
    pub fn conflicts(&self, a: Action, c: Chord) -> Vec<Action> {
        ACTIONS
            .iter()
            .copied()
            .filter(|&other| other != a && self.get(other) == c)
            .collect()
    }

    /// True once per press of the action's chord; consumes the key event so
    /// it doesn't also reach focused widgets.
    pub fn pressed(&self, ctx: &Context, a: Action) -> bool {
        let c = self.get(a);
        ctx.input_mut(|i| i.consume_key(c.mods, c.key))
    }

    pub fn text(&self, a: Action) -> String {
        self.get(a).text()
    }

    pub fn serialize(&self) -> String {
        ACTIONS
            .iter()
            .map(|a| format!("{}={}", a.storage_key(), self.get(*a).serialize()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn deserialize(s: &str) -> Self {
        let mut km = Self::default_map();
        for line in s.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let Some(action) = ACTIONS.iter().copied().find(|a| a.storage_key() == k) else {
                continue;
            };
            if let Some(chord) = Chord::parse(v) {
                km.set(action, chord);
            }
        }
        km
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut km = Keymap::default_map();
        km.set(
            Action::FindInFile,
            Chord::new(Modifiers::COMMAND | Modifiers::SHIFT, Key::G),
        );
        let restored = Keymap::deserialize(&km.serialize());
        assert_eq!(restored.get(Action::FindInFile).key, Key::G);
        assert!(restored.get(Action::FindInFile).mods.shift);
        // Untouched actions keep defaults.
        assert_eq!(restored.get(Action::Back).key, Key::OpenBracket);
    }

    #[test]
    fn conflict_detection() {
        let km = Keymap::default_map();
        let dup = km.get(Action::FindInFile);
        assert_eq!(km.conflicts(Action::GoToLine, dup), vec![Action::FindInFile]);
        assert!(km.conflicts(Action::FindInFile, dup).is_empty());
    }
}
