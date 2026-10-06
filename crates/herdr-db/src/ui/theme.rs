//! Colors and glyphs. Environments: local green, development blue, staging
//! yellow, production red. Glyphs use a Nerd Font when one is installed,
//! with an ASCII fallback.

use herdr_db_core::config::{Environment, IconMode};
use herdr_db_core::model::{Badges, ObjectKind};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use std::path::PathBuf;

pub fn env_color(environment: Environment) -> Color {
    match environment {
        Environment::Local => Color::Green,
        Environment::Development => Color::Blue,
        Environment::Staging => Color::Yellow,
        Environment::Production => Color::Red,
    }
}

pub const GOLD: Color = Color::Rgb(0xe5, 0xb5, 0x2a);
pub const FK_BLUE: Color = Color::Rgb(0x4a, 0x9e, 0xe8);

pub fn dim() -> Style {
    Style::new().fg(Color::DarkGray)
}

pub fn selected() -> Style {
    Style::new().add_modifier(Modifier::REVERSED)
}

pub fn null_style() -> Style {
    Style::new().fg(Color::DarkGray).add_modifier(Modifier::ITALIC)
}

pub fn error() -> Style {
    Style::new().fg(Color::Red)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Icons {
    pub nerd: bool,
}

impl Icons {
    pub fn new(mode: IconMode) -> Icons {
        let nerd = match mode {
            IconMode::NerdFont => true,
            IconMode::Ascii => false,
            IconMode::Auto => match std::env::var("HERDR_DB_ICONS").as_deref() {
                Ok("ascii") => false,
                Ok("nerd-font") => true,
                _ => nerd_font_installed(),
            },
        };
        Icons { nerd }
    }

    fn pick(&self, nerd: &'static str, ascii: &'static str) -> &'static str {
        if self.nerd { nerd } else { ascii }
    }

    pub fn folder(&self, open: bool) -> &'static str {
        if open { self.pick("\u{f07c}", "+") } else { self.pick("\u{f07b}", "+") }
    }
    pub fn source(&self) -> &'static str {
        self.pick("\u{f1c0}", "@")
    }
    pub fn schema(&self) -> &'static str {
        self.pick("\u{ea8b}", "#")
    }
    pub fn group(&self) -> &'static str {
        self.pick("\u{f114}", "")
    }
    pub fn object(&self, kind: ObjectKind) -> &'static str {
        match kind {
            ObjectKind::View => self.pick("\u{f06e}", "V"),
            ObjectKind::MaterializedView => self.pick("\u{f06e}", "M"),
            ObjectKind::ForeignTable => self.pick("\u{f0c1}", "F"),
            _ => self.pick("\u{f0ce}", "T"),
        }
    }
    pub fn column(&self) -> &'static str {
        self.pick("\u{f0db}", "")
    }
    pub fn key(&self) -> &'static str {
        self.pick("\u{f084}", "K")
    }
    pub fn index(&self) -> &'static str {
        self.pick("\u{f0e7}", "IX")
    }
    pub fn trigger(&self) -> &'static str {
        self.pick("\u{f1e6}", "TR")
    }
    pub fn check(&self) -> &'static str {
        self.pick("\u{f00c}", "CK")
    }
    pub fn expanded(&self) -> &'static str {
        self.pick("▾", "v")
    }
    pub fn collapsed(&self) -> &'static str {
        self.pick("▸", ">")
    }
    pub fn spinner(&self, tick: usize) -> &'static str {
        const NERD: [&str; 8] = ["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"];
        const ASCII: [&str; 4] = ["|", "/", "-", "\\"];
        if self.nerd { NERD[tick % NERD.len()] } else { ASCII[tick % ASCII.len()] }
    }

    /// Column badges, combined like IntelliJ: primary key (gold key),
    /// foreign key (blue key), indexed (bolt), NOT NULL (full dot).
    pub fn badges(&self, badges: Badges) -> Vec<Span<'static>> {
        let mut spans = Vec::new();
        let push = |spans: &mut Vec<Span<'static>>, text: &'static str, color: Color| {
            spans.push(Span::styled(text, Style::new().fg(color)));
        };
        if self.nerd {
            if badges.primary_key {
                push(&mut spans, "\u{f084}", GOLD);
            }
            if badges.foreign_key {
                push(&mut spans, "\u{f084}", FK_BLUE);
            }
            if badges.indexed && !badges.primary_key {
                push(&mut spans, "\u{f0e7}", Color::Cyan);
            }
            if badges.not_null && !badges.primary_key {
                push(&mut spans, "●", Color::Gray);
            }
        } else {
            let mut parts = Vec::new();
            if badges.primary_key {
                parts.push(("PK", GOLD));
            }
            if badges.foreign_key {
                parts.push(("FK", FK_BLUE));
            }
            if badges.indexed && !badges.primary_key {
                parts.push(("IX", Color::Cyan));
            }
            if badges.not_null && !badges.primary_key {
                parts.push(("!", Color::Gray));
            }
            for (i, (text, color)) in parts.into_iter().enumerate() {
                if i > 0 {
                    spans.push(Span::raw(" "));
                }
                push(&mut spans, text, color);
            }
        }
        spans
    }
}

/// Heuristic: a font file named "Nerd Font"/"NF" in the usual places. The
/// terminal may use another font; `icons` in the config overrides it.
fn nerd_font_installed() -> bool {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let dirs = [
        home.join("Library/Fonts"),
        PathBuf::from("/Library/Fonts"),
        home.join(".local/share/fonts"),
        home.join(".fonts"),
        PathBuf::from("/usr/share/fonts"),
        PathBuf::from("/usr/local/share/fonts"),
    ];
    let is_nerd = |name: &str| {
        let lower = name.to_ascii_lowercase();
        lower.contains("nerd") || lower.contains("nerdfont") || lower.ends_with("nf.ttf") || lower.contains(" nf ")
    };
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_nerd(&name) {
                return true;
            }
            if entry.path().is_dir()
                && let Ok(inner) = std::fs::read_dir(entry.path())
                && inner.flatten().any(|e| is_nerd(&e.file_name().to_string_lossy()))
            {
                return true;
            }
        }
    }
    false
}
