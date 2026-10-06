//! Configurable keymap. The IntelliJ keymap is not reused: Herdr's default
//! prefix is ctrl+b, IntelliJ's Go to DDL. Every action can be rebound in the
//! `[keys]` table of the config (`edit_data = "E"`, `execute = "ctrl+r, f5"`).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeySpec {
    code: KeyCode,
    modifiers: KeyModifiers,
}

impl KeySpec {
    pub fn parse(text: &str) -> Option<KeySpec> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        // A lone "+" or "-" is a key, not a separator.
        let (mods, key) = match text.rfind('+') {
            Some(i) if i > 0 && i < text.len() - 1 => (&text[..i], &text[i + 1..]),
            _ => ("", text),
        };
        let mut modifiers = KeyModifiers::NONE;
        for m in mods.split('+').filter(|m| !m.is_empty()) {
            modifiers |= match m.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => KeyModifiers::CONTROL,
                "alt" | "meta" | "option" => KeyModifiers::ALT,
                "shift" => KeyModifiers::SHIFT,
                _ => return None,
            };
        }
        let code = match key.to_ascii_lowercase().as_str() {
            "enter" | "return" => KeyCode::Enter,
            "esc" | "escape" => KeyCode::Esc,
            "tab" if modifiers.contains(KeyModifiers::SHIFT) => {
                modifiers.remove(KeyModifiers::SHIFT);
                KeyCode::BackTab
            }
            "tab" => KeyCode::Tab,
            "backtab" => KeyCode::BackTab,
            "space" => KeyCode::Char(' '),
            "backspace" => KeyCode::Backspace,
            "delete" | "del" => KeyCode::Delete,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pageup" | "pgup" => KeyCode::PageUp,
            "pagedown" | "pgdn" => KeyCode::PageDown,
            f if f.len() >= 2 && f.starts_with('f') && f[1..].parse::<u8>().is_ok() => {
                KeyCode::F(f[1..].parse().expect("checked"))
            }
            _ if key.chars().count() == 1 => {
                let c = key.chars().next().expect("one char");
                // `shift+k` and `K` are the same key.
                if modifiers.contains(KeyModifiers::SHIFT) && c.is_ascii_alphabetic() {
                    modifiers.remove(KeyModifiers::SHIFT);
                    KeyCode::Char(c.to_ascii_uppercase())
                } else if modifiers.contains(KeyModifiers::CONTROL) || modifiers.contains(KeyModifiers::ALT) {
                    KeyCode::Char(c.to_ascii_lowercase())
                } else {
                    KeyCode::Char(c)
                }
            }
            _ => return None,
        };
        Some(KeySpec { code, modifiers })
    }

    pub fn matches(&self, key: &KeyEvent) -> bool {
        let mut modifiers = key.modifiers;
        let mut code = key.code;
        if let KeyCode::Char(c) = code {
            // Terminals report shifted letters as uppercase, sometimes with SHIFT.
            modifiers.remove(KeyModifiers::SHIFT);
            if modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
                code = KeyCode::Char(c.to_ascii_lowercase());
            }
        }
        if code == KeyCode::BackTab {
            modifiers.remove(KeyModifiers::SHIFT);
        }
        code == self.code && modifiers == self.modifiers
    }

    pub fn label(&self) -> String {
        let mut out = String::new();
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            out.push_str("ctrl+");
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            out.push_str("alt+");
        }
        out.push_str(&match self.code {
            KeyCode::Char(' ') => "space".to_string(),
            KeyCode::Char(c) => c.to_string(),
            KeyCode::Enter => "Entrée".to_string(),
            KeyCode::Esc => "Échap".to_string(),
            KeyCode::Tab => "Tab".to_string(),
            KeyCode::BackTab => "shift+Tab".to_string(),
            KeyCode::F(n) => format!("F{n}"),
            KeyCode::Up => "↑".to_string(),
            KeyCode::Down => "↓".to_string(),
            KeyCode::Left => "←".to_string(),
            KeyCode::Right => "→".to_string(),
            KeyCode::PageUp => "PgUp".to_string(),
            KeyCode::PageDown => "PgDn".to_string(),
            other => format!("{other:?}"),
        });
        out
    }
}

/// Default bindings, by action name. Panes only look at their own actions,
/// so the same key can mean different things in different panes.
const DEFAULTS: &[(&str, &str)] = &[
    // Navigation, shared.
    ("up", "k, up"),
    ("down", "j, down"),
    ("left", "h, left"),
    ("right", "l, right"),
    ("top", "g, home"),
    ("bottom", "G, end"),
    ("page_up", "ctrl+u, pageup"),
    ("page_down", "ctrl+d, pagedown"),
    ("quit", "q"),
    ("help", "?"),
    ("cancel", "ctrl+c"),
    // Tree.
    ("expand", "l, right, enter"),
    ("collapse", "h, left"),
    ("search", "/"),
    ("edit_data", "e"),
    ("ddl", "d"),
    ("quickdoc", "K"),
    ("console", "c"),
    ("copy_reference", "y"),
    ("refresh", "r"),
    ("force_refresh", "R"),
    ("schemas", "s"),
    ("new_source", "n"),
    // Grid.
    ("next_page", "], n"),
    ("prev_page", "[, p"),
    ("first_page", "{"),
    ("last_page", "}"),
    ("sort", "s"),
    ("filter", "f, /"),
    ("inspect", "enter"),
    ("follow_fk", "F"),
    ("copy_cell", "y"),
    ("copy_csv", "Y"),
    ("copy_json", "J"),
    ("select", "v"),
    ("count", "C"),
    ("reload", "r"),
    // Console.
    ("execute", "ctrl+r, f5, ctrl+enter, alt+enter"),
    ("switch_focus", "tab"),
    ("write_mode", "ctrl+w"),
    ("history_prev", "ctrl+up, ctrl+p"),
    ("history_next", "ctrl+down, ctrl+n"),
    ("close", "ctrl+q"),
];

#[derive(Debug, Clone)]
pub struct Keymap {
    bindings: BTreeMap<String, Vec<KeySpec>>,
}

fn parse_list(text: &str) -> Vec<KeySpec> {
    // "," separates keys; the comma key itself is written "comma".
    text.split(',').map(str::trim).filter_map(|k| KeySpec::parse(if k == "comma" { "," } else { k })).collect()
}

impl Keymap {
    pub fn new(overrides: &BTreeMap<String, String>) -> Keymap {
        let mut bindings: BTreeMap<String, Vec<KeySpec>> =
            DEFAULTS.iter().map(|(action, keys)| (action.to_string(), parse_list(keys))).collect();
        for (action, keys) in overrides {
            bindings.insert(action.clone(), parse_list(keys));
        }
        Keymap { bindings }
    }

    pub fn is(&self, action: &str, key: &KeyEvent) -> bool {
        self.bindings.get(action).is_some_and(|specs| specs.iter().any(|s| s.matches(key)))
    }

    /// First action of `actions` bound to `key`.
    pub fn find<'a>(&self, actions: &[&'a str], key: &KeyEvent) -> Option<&'a str> {
        actions.iter().copied().find(|a| self.is(a, key))
    }

    pub fn label(&self, action: &str) -> String {
        self.bindings.get(action).and_then(|specs| specs.first()).map(KeySpec::label).unwrap_or_else(|| "—".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn parses_and_matches() {
        let ctrl_r = KeySpec::parse("ctrl+r").unwrap();
        assert!(ctrl_r.matches(&key(KeyCode::Char('r'), KeyModifiers::CONTROL)));
        assert!(!ctrl_r.matches(&key(KeyCode::Char('r'), KeyModifiers::NONE)));
        let big_k = KeySpec::parse("K").unwrap();
        assert!(big_k.matches(&key(KeyCode::Char('K'), KeyModifiers::SHIFT)));
        assert!(big_k.matches(&key(KeyCode::Char('K'), KeyModifiers::NONE)));
        assert!(!big_k.matches(&key(KeyCode::Char('k'), KeyModifiers::NONE)));
        assert_eq!(KeySpec::parse("shift+k"), KeySpec::parse("K"));
        assert!(KeySpec::parse("f5").unwrap().matches(&key(KeyCode::F(5), KeyModifiers::NONE)));
        assert!(KeySpec::parse("/").unwrap().matches(&key(KeyCode::Char('/'), KeyModifiers::NONE)));
        assert!(KeySpec::parse("+").is_some());
        assert!(KeySpec::parse("hyper+x").is_none());
    }

    #[test]
    fn overrides_replace_defaults() {
        let overrides = BTreeMap::from([("edit_data".to_string(), "E, ctrl+e".to_string())]);
        let keymap = Keymap::new(&overrides);
        assert!(!keymap.is("edit_data", &key(KeyCode::Char('e'), KeyModifiers::NONE)));
        assert!(keymap.is("edit_data", &key(KeyCode::Char('E'), KeyModifiers::SHIFT)));
        assert!(keymap.is("edit_data", &key(KeyCode::Char('e'), KeyModifiers::CONTROL)));
        assert!(keymap.is("next_page", &key(KeyCode::Char(']'), KeyModifiers::NONE)));
        assert_eq!(keymap.find(&["ddl", "quickdoc"], &key(KeyCode::Char('K'), KeyModifiers::SHIFT)), Some("quickdoc"));
        assert_eq!(keymap.label("execute"), "ctrl+r");
    }
}
