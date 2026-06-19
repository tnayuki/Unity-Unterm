//! Terminal color resolution: ANSI/256/truecolor -> RGB.
//!
//! The VT model stores each cell's color as a `vte::ansi::Color` — a named
//! ANSI slot, a 256-color palette index, or a direct truecolor spec. The host
//! supplies the themeable parts (default fg/bg/cursor and the 16 ANSI colors,
//! typically derived from the Unity editor skin); everything else is the
//! standard xterm 6x6x6 cube + grayscale ramp.

use alacritty_terminal::vte::ansi::Color;

/// The themeable colors. Defaults are a dark scheme close to common terminals.
#[derive(Clone, Copy)]
pub struct Theme {
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    pub cursor: [u8; 3],
    /// Selection highlight background (derived from bg/fg, see [`selection_bg`]).
    pub selection: [u8; 3],
    /// The 16 base ANSI colors (0..7 normal, 8..15 bright).
    pub ansi: [[u8; 3]; 16],
}

/// Derive a selection-highlight background from the terminal background: blend
/// it toward a blue accent so the result adapts to dark/light themes while
/// staying a distinct, legible tint under the (unchanged) cell text color.
pub fn selection_bg(bg: [u8; 3]) -> [u8; 3] {
    const ACCENT: [u8; 3] = [0x4d, 0x7a, 0xc7];
    const T: f32 = 0.55;
    let mix = |a: u8, b: u8| (a as f32 * (1.0 - T) + b as f32 * T).round() as u8;
    [mix(bg[0], ACCENT[0]), mix(bg[1], ACCENT[1]), mix(bg[2], ACCENT[2])]
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            fg: [0xd0, 0xd0, 0xd4],
            bg: [0x12, 0x12, 0x12],
            cursor: [0xd0, 0xd0, 0xd4],
            selection: selection_bg([0x12, 0x12, 0x12]),
            // A classic, legible 16-color set.
            ansi: [
                [0x1d, 0x1f, 0x21], // black
                [0xcc, 0x66, 0x66], // red
                [0xb5, 0xbd, 0x68], // green
                [0xf0, 0xc6, 0x74], // yellow
                [0x81, 0xa2, 0xbe], // blue
                [0xb2, 0x94, 0xbb], // magenta
                [0x8a, 0xbe, 0xb7], // cyan
                [0xc5, 0xc8, 0xc6], // white
                [0x66, 0x66, 0x66], // bright black
                [0xd5, 0x4e, 0x53], // bright red
                [0xb9, 0xca, 0x4a], // bright green
                [0xe7, 0xc5, 0x47], // bright yellow
                [0x7a, 0xa6, 0xda], // bright blue
                [0xc3, 0x97, 0xd8], // bright magenta
                [0x70, 0xc0, 0xb1], // bright cyan
                [0xea, 0xea, 0xea], // bright white
            ],
        }
    }
}

/// Resolve a VT cell color to RGB against the theme.
pub fn resolve(color: Color, theme: &Theme) -> [u8; 3] {
    match color {
        Color::Spec(rgb) => [rgb.r, rgb.g, rgb.b],
        Color::Indexed(i) => indexed(i, theme),
        Color::Named(named) => {
            let idx = named as usize;
            match idx {
                0..=15 => theme.ansi[idx],
                256 => theme.fg,                          // Foreground
                257 => theme.bg,                          // Background
                258 => theme.cursor,                      // Cursor
                259..=266 => dim(theme.ansi[idx - 259]),  // DimBlack..DimWhite
                267 => theme.fg,                          // BrightForeground
                268 => dim(theme.fg),                     // DimForeground
                _ => theme.fg,
            }
        }
    }
}

/// Map a 0..255 palette index to RGB: 16 base + 6x6x6 cube + grayscale ramp.
fn indexed(i: u8, theme: &Theme) -> [u8; 3] {
    match i {
        0..=15 => theme.ansi[i as usize],
        16..=231 => {
            let i = i - 16;
            let r = i / 36;
            let g = (i % 36) / 6;
            let b = i % 6;
            [cube(r), cube(g), cube(b)]
        }
        _ => {
            // 232..=255: 24-step grayscale ramp.
            let v = 8 + (i - 232) * 10;
            [v, v, v]
        }
    }
}

/// xterm color-cube step (0,95,135,175,215,255).
fn cube(n: u8) -> u8 {
    if n == 0 {
        0
    } else {
        55 + n * 40
    }
}

/// Dim a color toward black (used for the dim ANSI variants).
fn dim(c: [u8; 3]) -> [u8; 3] {
    [
        (c[0] as u16 * 2 / 3) as u8,
        (c[1] as u16 * 2 / 3) as u8,
        (c[2] as u16 * 2 / 3) as u8,
    ]
}
