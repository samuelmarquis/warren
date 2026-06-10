//! Styled text runs — the unit of screen content on the wire.
//!
//! Daemons render their embedded terminal's cells into `Span` runs; viewers
//! draw them verbatim and never parse escape sequences. Conversion from
//! alacritty cells lives with the daemon (M3); this module is dependency-free
//! so the dashboard and tests can share it.

use serde::{Deserialize, Serialize};

pub mod attr {
    pub const BOLD: u8 = 1 << 0;
    pub const DIM: u8 = 1 << 1;
    pub const ITALIC: u8 = 1 << 2;
    pub const UNDERLINE: u8 = 1 << 3;
    pub const INVERSE: u8 = 1 << 4;
    pub const STRIKEOUT: u8 = 1 << 5;
    pub const HIDDEN: u8 = 1 << 6;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub text: String,
    #[serde(default, skip_serializing_if = "is_default_color")]
    pub fg: Color,
    #[serde(default, skip_serializing_if = "is_default_color")]
    pub bg: Color,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub attrs: u8,
}

fn is_default_color(c: &Color) -> bool {
    *c == Color::Default
}
fn is_zero(v: &u8) -> bool {
    *v == 0
}

impl Span {
    pub fn plain(text: impl Into<String>) -> Self {
        Span { text: text.into(), fg: Color::Default, bg: Color::Default, attrs: 0 }
    }
}

/// One terminal row as a sequence of styled runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LineSpans(pub Vec<Span>);

impl LineSpans {
    /// Total character width (assumes 1 column per char; wide glyphs are the
    /// renderer's concern).
    pub fn width(&self) -> usize {
        self.0.iter().map(|s| s.text.chars().count()).sum()
    }
}

/// Build the SGR escape selecting a span's style (always starts from reset,
/// so spans are self-contained).
pub fn sgr_sequence(span: &Span) -> String {
    let mut out = String::from("\x1b[0");
    if span.attrs & attr::BOLD != 0 {
        out.push_str(";1");
    }
    if span.attrs & attr::DIM != 0 {
        out.push_str(";2");
    }
    if span.attrs & attr::ITALIC != 0 {
        out.push_str(";3");
    }
    if span.attrs & attr::UNDERLINE != 0 {
        out.push_str(";4");
    }
    if span.attrs & attr::INVERSE != 0 {
        out.push_str(";7");
    }
    if span.attrs & attr::HIDDEN != 0 {
        out.push_str(";8");
    }
    if span.attrs & attr::STRIKEOUT != 0 {
        out.push_str(";9");
    }
    match span.fg {
        Color::Default => {}
        Color::Indexed(i) => out.push_str(&format!(";38;5;{i}")),
        Color::Rgb(r, g, b) => out.push_str(&format!(";38;2;{r};{g};{b}")),
    }
    match span.bg {
        Color::Default => {}
        Color::Indexed(i) => out.push_str(&format!(";48;5;{i}")),
        Color::Rgb(r, g, b) => out.push_str(&format!(";48;2;{r};{g};{b}")),
    }
    out.push('m');
    out
}

/// Resolve an xterm-256 palette index to RGB (standard xterm values:
/// 16 named, 6x6x6 cube, 24-step grayscale ramp).
pub fn xterm256_to_rgb(idx: u8) -> (u8, u8, u8) {
    const BASE: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 0, 0),
        (0, 205, 0),
        (205, 205, 0),
        (0, 0, 238),
        (205, 0, 205),
        (0, 205, 205),
        (229, 229, 229),
        (127, 127, 127),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (92, 92, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    match idx {
        0..=15 => BASE[idx as usize],
        16..=231 => {
            let i = idx - 16;
            let to_level = |v: u8| if v == 0 { 0 } else { 55 + 40 * v };
            (to_level(i / 36), to_level((i / 6) % 6), to_level(i % 6))
        }
        232..=255 => {
            let v = 8 + 10 * (idx - 232);
            (v, v, v)
        }
    }
}

/// Rec.601 luma test: should text on this background be white?
/// (Same policy as v0 dvtm's color_is_dark.)
pub fn color_is_dark(r: u8, g: u8, b: u8) -> bool {
    let luma = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
    luma < 128.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xterm_cube_and_ramp() {
        assert_eq!(xterm256_to_rgb(16), (0, 0, 0)); // cube origin
        assert_eq!(xterm256_to_rgb(231), (255, 255, 255)); // cube max
        assert_eq!(xterm256_to_rgb(196), (255, 0, 0)); // pure red in cube
        assert_eq!(xterm256_to_rgb(232), (8, 8, 8)); // ramp start
        assert_eq!(xterm256_to_rgb(255), (238, 238, 238)); // ramp end
    }

    #[test]
    fn dark_vs_light() {
        assert!(color_is_dark(0, 0, 0));
        assert!(!color_is_dark(255, 255, 255));
        assert!(color_is_dark(0, 0, 255)); // saturated blue is dark
        assert!(!color_is_dark(255, 255, 0)); // yellow is light
    }
}
