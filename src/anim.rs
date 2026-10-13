//! Terminal animation layer for suggestion rendering.
//!
//! termplete renders *inside someone else's terminal*, under zsh's ZLE or bash's
//! readline. That constrains the design: a full-screen TUI framework (Bubble Tea,
//! Ratatui, Textual and friends) owns the alternate screen, the input loop and
//! the whole cell grid, which is fundamentally incompatible with letting a shell
//! keep drawing its own prompt. So this layer is deliberately small and
//! self-contained — precomputed ANSI frames that a shell plugin splices into the
//! ghost-text region and the status line.
//!
//! - **Ghost shimmer** — ghost text cycles along a color gradient. Each keystroke
//!   advances the frame, so the pulse is driven by typing cadence rather than by
//!   a repaint loop.
//!
//! Everything is gated: [`Anim::disabled`] returns borrowed text unchanged, so
//! turning animations off costs nothing. `NO_COLOR`, a dumb terminal,
//! `TERMPLETE_NO_ANIM`, and `animations = false` in config all disable it.

use std::borrow::Cow;

use crate::config::StyleConfig;

/// Default number of distinct frames in a shimmer cycle.
pub const DEFAULT_FRAMES: usize = 12;

/// 8-bit RGB color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    /// Parse `#rgb`, `#rrggbb` or bare hex. Returns `None` on malformed input so
    /// a typo in config falls back to the default rather than emitting junk.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let hex = s.strip_prefix('#').unwrap_or(s);
        match hex.len() {
            3 => {
                let mut it = hex.chars();
                let r = it.next()?.to_digit(16)? as u8;
                let g = it.next()?.to_digit(16)? as u8;
                let b = it.next()?.to_digit(16)? as u8;
                Some(Rgb {
                    r: r * 17,
                    g: g * 17,
                    b: b * 17,
                })
            }
            6 => {
                let v = u32::from_str_radix(hex, 16).ok()?;
                Some(Rgb {
                    r: ((v >> 16) & 0xff) as u8,
                    g: ((v >> 8) & 0xff) as u8,
                    b: (v & 0xff) as u8,
                })
            }
            _ => None,
        }
    }
}

/// A linear two-stop color gradient used for the shimmer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gradient {
    pub from: Rgb,
    pub to: Rgb,
}

impl Gradient {
    /// Interpolate the gradient at `t` in `[0, 1]`, clamped.
    pub fn at(&self, t: f32) -> Rgb {
        let t = t.clamp(0.0, 1.0);
        Rgb {
            r: lerp(self.from.r, self.to.r, t),
            g: lerp(self.from.g, self.to.g, t),
            b: lerp(self.from.b, self.to.b, t),
        }
    }

    /// Precompute every frame's SGR pair so rendering is an index plus a copy.
    ///
    /// The phase ping-pongs so the pulse eases in and out. The last frame
    /// repeats the first color, so wrapping back to frame zero is a no-op
    /// rather than a visible jump.
    pub fn frames(&self, count: usize) -> Vec<(String, String)> {
        let count = count.max(2);
        (0..count)
            .map(|i| {
                let phase = i as f32 / (count - 1) as f32;
                let t = if phase <= 0.5 {
                    phase * 2.0
                } else {
                    (1.0 - phase) * 2.0
                };
                let c = self.at(t);
                (
                    format!("\x1b[38;2;{};{};{}m", c.r, c.g, c.b),
                    RESET_FG.to_string(),
                )
            })
            .collect()
    }
}

const RESET_FG: &str = "\x1b[39m";

/// Return ANSI-256 colors for the shell's animated loading label.
///
/// The seven entries are muted color, accent color, contrasting ink (0 or 15),
/// then four ease-out foreground frames from accent to muted. RGB colors are
/// quantized only against ANSI-256 entries 16–255 so terminal-specific colors
/// 0–15 remain untouched.
pub fn zle_palette(style: &StyleConfig) -> Vec<u8> {
    let muted = Rgb::parse(&style.ghost_color).unwrap_or(Rgb {
        r: 0x6b,
        g: 0x72,
        b: 0x80,
    });
    let accent = Rgb::parse(&style.eager_color).unwrap_or(Rgb {
        r: 0x8b,
        g: 0x5c,
        b: 0xf6,
    });
    let ink = contrasting_ink(muted, accent);
    let frames = Gradient {
        from: accent,
        to: muted,
    };
    let frame_indices = (0..4).map(|i| {
        let t = i as f32 / 3.0;
        let eased = 1.0 - (1.0 - t) * (1.0 - t);
        nearest_ansi256(frames.at(eased))
    });

    [nearest_ansi256(muted), nearest_ansi256(accent), ink]
        .into_iter()
        .chain(frame_indices)
        .collect()
}

fn nearest_ansi256(color: Rgb) -> u8 {
    (16u16..=255)
        .min_by_key(|index| color_distance(color, ansi256_rgb(*index as u8)))
        .unwrap_or(16) as u8
}

fn ansi256_rgb(index: u8) -> Rgb {
    const CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];
    if index < 232 {
        let value = (index - 16) as usize;
        Rgb {
            r: CUBE[value / 36],
            g: CUBE[(value / 6) % 6],
            b: CUBE[value % 6],
        }
    } else {
        let gray = 8 + (index - 232) * 10;
        Rgb {
            r: gray,
            g: gray,
            b: gray,
        }
    }
}

fn color_distance(a: Rgb, b: Rgb) -> u32 {
    let dr = a.r as i32 - b.r as i32;
    let dg = a.g as i32 - b.g as i32;
    let db = a.b as i32 - b.b as i32;
    (dr * dr + dg * dg + db * db) as u32
}

fn contrasting_ink(muted: Rgb, accent: Rgb) -> u8 {
    let black = Rgb { r: 0, g: 0, b: 0 };
    let white = Rgb {
        r: 255,
        g: 255,
        b: 255,
    };
    let black_score = contrast_ratio(muted, black).min(contrast_ratio(accent, black));
    let white_score = contrast_ratio(muted, white).min(contrast_ratio(accent, white));
    if white_score > black_score {
        15
    } else {
        0
    }
}

fn contrast_ratio(a: Rgb, b: Rgb) -> f32 {
    let a = relative_luminance(a);
    let b = relative_luminance(b);
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

fn relative_luminance(color: Rgb) -> f32 {
    fn linearize(channel: u8) -> f32 {
        let channel = channel as f32 / 255.0;
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    }

    0.2126 * linearize(color.r) + 0.7152 * linearize(color.g) + 0.0722 * linearize(color.b)
}

fn lerp(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t)
        .round()
        .clamp(0.0, 255.0) as u8
}

/// The animation configuration for one process, resolved from user config and
/// the environment.
///
/// Construct once per suggestion and reuse; the frame tables are precomputed.
#[derive(Debug, Clone)]
pub struct Anim {
    enabled: bool,
    /// Precomputed SGR pairs indexed by frame.
    ghost_frames: Vec<(String, String)>,
    /// Precomputed SGR pairs for eager-mode ghost text.
    eager_frames: Vec<(String, String)>,
}

impl Anim {
    /// Build from style config, honoring the user's toggles and the environment.
    ///
    /// Animations are off when the config says so, when `NO_COLOR` is set, when
    /// the terminal is dumb, or when `TERMPLETE_NO_ANIM` is truthy.
    pub fn from_style(style: &StyleConfig) -> Self {
        let muted = Rgb::parse(&style.ghost_color).unwrap_or(Rgb {
            r: 0x6b,
            g: 0x72,
            b: 0x80,
        });
        let bright = Rgb::parse(&style.eager_color).unwrap_or(Rgb {
            r: 0x8b,
            g: 0x5c,
            b: 0xf6,
        });
        let count = style.animation_frames.clamp(2, 240);
        // The shimmer travels from the muted tone toward the bright one; eager
        // ghost text runs the gradient the other way so the two modes read
        // differently at a glance.
        let ghost = Gradient {
            from: muted,
            to: bright,
        }
        .frames(count);
        let eager = Gradient {
            from: bright,
            to: muted,
        }
        .frames(count);
        Self {
            enabled: style.animations && animations_allowed(),
            ghost_frames: ghost,
            eager_frames: eager,
        }
    }

    /// An always-off instance. Rendering borrows, so this costs nothing.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ghost_frames: Vec::new(),
            eager_frames: Vec::new(),
        }
    }

    #[cfg(test)]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Render `text` as ghost text at `frame`.
    ///
    /// Returns `Cow::Borrowed` when animations or color are off, which keeps the
    /// common non-animated path allocation-free.
    pub fn ghost<'a>(&'a self, text: &'a str, frame: usize, eager: bool) -> Cow<'a, str> {
        if !self.enabled || text.is_empty() {
            return Cow::Borrowed(text);
        }
        let frames = if eager {
            &self.eager_frames
        } else {
            &self.ghost_frames
        };
        if frames.is_empty() {
            return Cow::Borrowed(text);
        }
        let (pre, post) = &frames[frame % frames.len()];
        Cow::Owned(format!("{pre}{text}{post}"))
    }
}

/// Whether animation and color are permitted at all.
fn animations_allowed() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if let Ok(term) = std::env::var("TERM") {
        if term.is_empty() || term == "dumb" {
            return false;
        }
    }
    !matches!(
        std::env::var("TERMPLETE_NO_ANIM").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn style() -> StyleConfig {
        StyleConfig::default()
    }

    #[test]
    fn parses_six_digit_hex() {
        assert_eq!(
            Rgb::parse("#8b5cf6"),
            Some(Rgb {
                r: 0x8b,
                g: 0x5c,
                b: 0xf6
            })
        );
    }

    #[test]
    fn parses_three_digit_hex() {
        assert_eq!(
            Rgb::parse("#fff"),
            Some(Rgb {
                r: 255,
                g: 255,
                b: 255
            })
        );
    }

    #[test]
    fn parses_bare_hex() {
        assert_eq!(Rgb::parse("000000"), Some(Rgb { r: 0, g: 0, b: 0 }));
    }

    #[test]
    fn rejects_malformed_colors() {
        assert_eq!(Rgb::parse("#12345"), None);
        assert_eq!(Rgb::parse("nope"), None);
    }

    #[test]
    fn zle_palette_uses_config_colors_and_eases_from_accent_to_muted() {
        let style = StyleConfig {
            ghost_color: "#00ff00".into(),
            eager_color: "#0000ff".into(),
            ..style()
        };
        let palette = zle_palette(&style);
        assert_eq!(palette.len(), 7);
        assert_eq!(palette[0], 46); // ANSI-256 green
        assert_eq!(palette[1], 21); // ANSI-256 blue
        assert!(matches!(palette[2], 0 | 15));
        assert_eq!(palette[3], palette[1], "first frame starts at accent");
        assert_eq!(palette[6], palette[0], "last frame reaches muted");
        assert!(palette[3..].iter().all(|index| (16..=255).contains(index)));

        let muted = ansi256_rgb(palette[0]);
        let distances: Vec<_> = palette[3..]
            .iter()
            .map(|index| color_distance(ansi256_rgb(*index), muted))
            .collect();
        assert!(distances.windows(2).all(|pair| pair[0] > pair[1]));
    }

    #[test]
    fn zle_palette_uses_contrasting_black_or_white_ink() {
        let light = StyleConfig {
            ghost_color: "#eeeeee".into(),
            eager_color: "#ffffff".into(),
            ..style()
        };
        let dark = StyleConfig {
            ghost_color: "#000000".into(),
            eager_color: "#111111".into(),
            ..style()
        };
        assert_eq!(zle_palette(&light)[2], 0);
        assert_eq!(zle_palette(&dark)[2], 15);
    }

    #[test]
    fn zle_palette_restricts_theme_colors_to_ansi256_and_falls_back_safely() {
        assert_eq!(nearest_ansi256(Rgb { r: 0, g: 0, b: 0 }), 16);
        assert_eq!(
            nearest_ansi256(Rgb {
                r: 255,
                g: 255,
                b: 255
            }),
            231
        );

        let invalid = StyleConfig {
            ghost_color: "not a color".into(),
            eager_color: "#12345".into(),
            ..style()
        };
        assert_eq!(zle_palette(&invalid), zle_palette(&StyleConfig::default()));
    }

    #[test]
    fn gradient_endpoints_match_stops() {
        let g = Gradient {
            from: Rgb { r: 0, g: 0, b: 0 },
            to: Rgb {
                r: 255,
                g: 255,
                b: 255,
            },
        };
        assert_eq!(g.at(0.0), Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(
            g.at(1.0),
            Rgb {
                r: 255,
                g: 255,
                b: 255
            }
        );
        assert_eq!(
            g.at(0.5),
            Rgb {
                r: 128,
                g: 128,
                b: 128
            }
        );
    }

    #[test]
    fn gradient_clamps_out_of_range_t() {
        let g = Gradient {
            from: Rgb {
                r: 10,
                g: 10,
                b: 10,
            },
            to: Rgb {
                r: 20,
                g: 20,
                b: 20,
            },
        };
        assert_eq!(g.at(-5.0).r, 10);
        assert_eq!(g.at(9.0).r, 20);
    }

    #[test]
    fn shimmer_cycle_closes_smoothly() {
        let g = Gradient {
            from: Rgb { r: 0, g: 0, b: 0 },
            to: Rgb {
                r: 100,
                g: 100,
                b: 100,
            },
        };
        let frames = g.frames(DEFAULT_FRAMES);
        assert_eq!(frames.len(), DEFAULT_FRAMES);
        assert_eq!(
            frames[0],
            frames[DEFAULT_FRAMES - 1],
            "cycle must close smoothly"
        );
    }

    #[test]
    fn shimmer_actually_varies_color() {
        let g = Gradient {
            from: Rgb { r: 0, g: 0, b: 0 },
            to: Rgb {
                r: 200,
                g: 200,
                b: 200,
            },
        };
        let frames = g.frames(DEFAULT_FRAMES);
        assert!(
            frames.iter().any(|f| f.0 != frames[0].0),
            "frames must differ"
        );
    }

    #[test]
    fn disabled_anim_borrows_without_allocating() {
        let anim = Anim::disabled();
        assert!(matches!(
            anim.ghost("text", 0, false),
            Cow::Borrowed("text")
        ));
    }

    #[test]
    fn empty_text_is_never_wrapped() {
        let anim = Anim {
            enabled: true,
            ghost_frames: Gradient {
                from: Rgb { r: 0, g: 0, b: 0 },
                to: Rgb { r: 1, g: 1, b: 1 },
            }
            .frames(DEFAULT_FRAMES),
            eager_frames: Vec::new(),
        };
        assert!(matches!(anim.ghost("", 0, false), Cow::Borrowed("")));
    }

    #[test]
    fn ghost_wraps_text_and_resets() {
        let anim = Anim {
            enabled: true,
            ghost_frames: vec![("\x1b[38;2;1;2;3m".into(), RESET_FG.into())],
            eager_frames: vec![("\x1b[38;2;4;5;6m".into(), RESET_FG.into())],
        };
        assert_eq!(anim.ghost("hi", 0, false), "\x1b[38;2;1;2;3mhi\x1b[39m");
        assert_eq!(anim.ghost("hi", 0, true), "\x1b[38;2;4;5;6mhi\x1b[39m");
    }

    #[test]
    fn ghost_wraps_any_frame_index() {
        let anim = Anim {
            enabled: true,
            ghost_frames: vec![("A".into(), "Z".into())],
            eager_frames: vec![("A".into(), "Z".into())],
        };
        assert_eq!(anim.ghost("x", 99, false), "AxZ", "index must wrap");
    }

    #[test]
    fn config_toggle_disables_animations() {
        let mut s = style();
        s.animations = false;
        assert!(!Anim::from_style(&s).is_enabled());
    }

    #[test]
    fn no_color_env_disables_animations() {
        std::env::set_var("NO_COLOR", "1");
        assert!(!animations_allowed());
        assert!(!Anim::from_style(&style()).is_enabled());
        std::env::remove_var("NO_COLOR");
    }

    #[test]
    fn explicit_opt_out_is_honored() {
        std::env::set_var("TERMPLETE_NO_ANIM", "1");
        assert!(!animations_allowed());
        std::env::remove_var("TERMPLETE_NO_ANIM");
    }

    #[test]
    fn dumb_terminal_disables_animations() {
        std::env::set_var("TERM", "dumb");
        assert!(!animations_allowed());
        std::env::remove_var("TERM");
    }
}
