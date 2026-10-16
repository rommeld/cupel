//! The TUI palette every style the transcript and chrome use, in one [`Theme`] value.
//!
//! As a struct, the visual hierarchy of turn is a single reviewable unit:
//!
//! `task` and `answer` are the emphasized endpoints of a turn (what was asked, what
//! came out).
//! Reasoning and tool traffic are the de-emphasized middle, errors/notices keep their
//! conventional terminal colors.
//!
//! A theme is data, not a set of global constants: the render pass gets the active
//! `&Theme` as a parameter and reads its fields, so a different theme restyles the
//! whole screen on the next frame.
//!
//! [`THEMES`] lists every shipped theme. [`CUPEL`] uses the terminal's own
//! colors; the others are built from a [`Palette`] and paint an opaque background.
//!
//! These are true `const`s: ratatui`s `Style::new()`, `fg`, `bg`, and `add_modifier`
//! are const fns. Note the chained `add_modifier` calls where two modifiers combine.
//! The `|` operator (BitOr) is not a const fn, so `Modifier::DIM | Modifier::ITALIC`
//! would not compile here.

use ratatui::style::{Color, Modifier, Style};

/// Every style the TUI paints, with, one field per role. A field names what the style
/// is for (`answer`), not how it looks (`magenta`), so each theme can pick its own
/// look for the same role.
#[derive(Debug)]
pub struct Theme {
    /// The lowercase id used by `/themes` and settings.json.
    pub id: &'static str,
    /// The name shown by `/themes`.
    pub name: &'static str,
    pub appearance: Appearance,
    /// Painted over every cell before other styles. Empty in `CUPEL`,
    /// which keeps the terminal's own foreground and background.
    pub base: Style,
    /// User messages in the conversation.
    pub task: Style,
    /// Mid-turn assistant prose (commentary between tool calls).
    pub assistant: Style,
    /// The turn's final answer: the emphasized counterpart to `task`.
    pub answer: Style,
    /// Model reasoning and context summaries.
    pub reasoning: Style,
    /// A tool call header (`$ cargo test`, `edite src/x.rs`).
    pub tool_header: Style,
    /// De-emphasized detail lines: pending markers, ok tool output, overflow, notes,
    /// usage summaries.
    pub detail: Style,
    /// Errors and failed tool results.
    pub error: Style,
    /// Status notices (retry, compaction, /provider listings).
    pub notice: Style,
    /// Diff lines insed a tool cell: added, removed, context. Green/red is the universal
    /// diff convention. The context stays receded.
    pub diff_add: Style,
    pub diff_del: Style,
    pub diff_ctx: Style,
    /// Input border while a run is active / while idle.
    pub input_border_busy: Style,
    pub input_border_idle: Style,
    /// Box titles and the footer line.
    pub chrome: Style,
    /// The " ↓ N more " overlay while scrolled up.
    pub scroll_marker: Style,
    /// Border and title of the transcript frame.
    pub pane_border: Style,
    /// Background of the click-selected conversation block (Ctrl+O copies it). Set only
    /// a background here: a Line's own style paints under its spans, so the highlight
    /// tints the row while evety span keeps its foreground color.
    pub selected: Style,
    /// The scrollbar thumb; the track reuses the pane border color.
    pub scrollbar_thumb: Style,
    /// Popup rows (autocomplete, the trust choice, the sidebar): the selected one, and the
    /// rest.
    pub popup_selected: Style,
    pub popup_row: Style,

    // Markdown patch styles: applied onto a cell's base style via Style::patch. Set fields
    // win, unset fields keep the base. A heading in an Answer cell is therefore in the
    // answer color; only styles that do a set color (code, links) deliberately break out
    // of the cell color, because code is code no matter which cell it is in.
    /// H1/H2: bold + underlined (H3-H6 get `md_bold` only depth fades).
    pub md_heading: Style,
    pub md_bold: Style,
    pub md_italic: Style,
    pub md_strike: Style,
    /// Inline code.
    pub md_code: Style,
    /// Fenced code blocks: a full-width panel. Not a patch: code blocks ignore the cell's
    /// style.
    pub md_code_block: Style,
    /// Blockquotes: receded like reasoning, but inside the cell's color.
    pub md_quote: Style,
    /// List bullets and ordered-list numbers.
    pub md_bullet: Style,
    /// Link text.
    pub md_link: Style,
    /// The "(url)" suffix behind a link whose text differs from its target.
    pub md_link_url: Style,
    /// Table separators and horizontal rules.
    pub md_rule: Style,
    /// Table header row.
    pub md_table_header: Style,
}

/// Which background a theme is made for, as shown by `/themes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appearance {
    /// Draws on the terminal's own background.
    Terminal,
    Dark,
    Light,
}

impl core::fmt::Display for Appearance {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Terminal => "terminal colors",
            Self::Dark => "dark",
            Self::Light => "light",
        })
    }
}

/// cupel's own look: the terminal's 16 named colors plus three gray surfaces from the
/// xtrem-256 palette.
pub const CUPEL: Theme = Theme {
    id: "cupel",
    name: "Cupel",
    appearance: Appearance::Terminal,
    base: Style::new(),
    // White on gray surface (xtrem-256 index 240, #585858).
    task: Style::new().fg(Color::White).bg(Color::Indexed(240)),
    assistant: Style::new().fg(Color::White).add_modifier(Modifier::ITALIC),
    // Magenta because cyan (tools), red (errors), and yellow (notice) are taken and
    // bold alone is too subtle next to plain prose.
    answer: Style::new().fg(Color::Magenta),
    reasoning: Style::new().fg(Color::White).add_modifier(Modifier::ITALIC),
    tool_header: Style::new().fg(Color::Cyan),
    detail: Style::new(),
    error: Style::new().fg(Color::Red),
    notice: Style::new().fg(Color::Yellow),
    // Green/red is the universal diff convention. The context says receded.
    diff_add: Style::new().fg(Color::Green),
    diff_del: Style::new().fg(Color::Red),
    diff_ctx: Style::new().fg(Color::DarkGray),
    input_border_busy: Style::new().fg(Color::Yellow),
    input_border_idle: Style::new().fg(Color::DarkGray),
    chrome: Style::new().add_modifier(Modifier::DIM),
    scroll_marker: Style::new().fg(Color::Black).bg(Color::Yellow),
    pane_border: Style::new().fg(Color::DarkGray),
    // xtrem-256 index 237 (#3a3a3a).
    selected: Style::new().bg(Color::Indexed(237)),
    scrollbar_thumb: Style::new().fg(Color::DarkGray),
    // The selected row inverts, the rest match the tool color.
    popup_selected: Style::new().add_modifier(Modifier::REVERSED),
    popup_row: Style::new().fg(Color::Cyan),
    md_heading: Style::new()
        .add_modifier(Modifier::BOLD)
        .add_modifier(Modifier::UNDERLINED),
    md_bold: Style::new().add_modifier(Modifier::BOLD),
    md_italic: Style::new().add_modifier(Modifier::ITALIC),
    md_strike: Style::new().add_modifier(Modifier::CROSSED_OUT),
    // cupel's cyan accent family (tools, popups).
    md_code: Style::new().fg(Color::Cyan),
    // xtrem-256 index 235 (#262626): subtler and more portable than truecolor, and
    // visually "a surface", not a color. The terminal's default forground stays.
    md_code_block: Style::new().bg(Color::Indexed(235)),
    md_quote: Style::new()
        .add_modifier(Modifier::DIM)
        .add_modifier(Modifier::ITALIC),
    md_bullet: Style::new().fg(Color::Cyan),
    // The universal terminal convention for links.
    md_link: Style::new()
        .fg(Color::Blue)
        .add_modifier(Modifier::UNDERLINED),
    md_link_url: Style::new().add_modifier(Modifier::DIM),
    md_rule: Style::new().add_modifier(Modifier::DIM),
    md_table_header: Style::new().add_modifier(Modifier::BOLD),
};

/// The 13 colors from which [`Theme::from_palette`] builds every style role.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// Screen background.
    pub bg: Color,
    /// Ordinary text.
    pub fg: Color,
    /// Code blocks and popups.
    pub surface: Color,
    /// User message background.
    pub raised: Color,
    /// Click-selected block background.
    pub selection: Color,
    /// Pane borders and idle input border.
    pub border: Color,
    /// Footer, titles, diff context, scrollbar thumb.
    pub muted: Color,
    /// Errors and removed diff lines.
    pub red: Color,
    /// Added diff lines.
    pub green: Color,
    /// Notices and busy input border.
    pub yellow: Color,
    /// Links.
    pub blue: Color,
    /// Final answer.
    pub magenta: Color,
    /// Tools, inline code, list bullets, popup rows.
    pub cyan: Color,
}

impl Theme {
    /// Build an opaque theme with `CUPEL`'s hierarchy in a palette's colors.
    /// Markdown modifiers remain patches onto the containing cell's style.
    #[must_use]
    pub const fn from_palette(
        id: &'static str,
        name: &'static str,
        appearance: Appearance,
        palette: Palette,
    ) -> Self {
        let Palette {
            bg,
            fg,
            surface,
            raised,
            selection,
            border,
            muted,
            red,
            green,
            yellow,
            blue,
            magenta,
            cyan,
        } = palette;
        Self {
            id,
            name,
            appearance,
            base: Style::new().fg(fg).bg(bg),
            task: Style::new().fg(fg).bg(raised),
            assistant: Style::new().fg(fg).add_modifier(Modifier::ITALIC),
            answer: Style::new().fg(magenta),
            reasoning: Style::new().fg(fg).add_modifier(Modifier::ITALIC),
            tool_header: Style::new().fg(cyan),
            detail: Style::new(),
            error: Style::new().fg(red),
            notice: Style::new().fg(yellow),
            diff_add: Style::new().fg(green),
            diff_del: Style::new().fg(red),
            diff_ctx: Style::new().fg(muted),
            input_border_busy: Style::new().fg(yellow),
            input_border_idle: Style::new().fg(border),
            // Explicit muted colors are consistent across terminals, unlike DIM.
            chrome: Style::new().fg(muted),
            scroll_marker: Style::new().fg(bg).bg(yellow),
            pane_border: Style::new().fg(border),
            selected: Style::new().bg(selection),
            scrollbar_thumb: Style::new().fg(muted),
            popup_selected: Style::new().add_modifier(Modifier::REVERSED),
            popup_row: Style::new().fg(cyan).bg(surface),
            md_heading: Style::new()
                .add_modifier(Modifier::BOLD)
                .add_modifier(Modifier::UNDERLINED),
            md_bold: Style::new().add_modifier(Modifier::BOLD),
            md_italic: Style::new().add_modifier(Modifier::ITALIC),
            md_strike: Style::new().add_modifier(Modifier::CROSSED_OUT),
            md_code: Style::new().fg(cyan),
            md_code_block: Style::new().bg(surface),
            md_quote: Style::new()
                .add_modifier(Modifier::DIM)
                .add_modifier(Modifier::ITALIC),
            md_bullet: Style::new().fg(cyan),
            md_link: Style::new().fg(blue).add_modifier(Modifier::UNDERLINED),
            md_link_url: Style::new().add_modifier(Modifier::DIM),
            md_rule: Style::new().add_modifier(Modifier::DIM),
            md_table_header: Style::new().add_modifier(Modifier::BOLD),
        }
    }
}

/// `#rrggbb` as a color, evaluated at compile time for the palettes below.
const fn hex(rgb: u32) -> Color {
    Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

// Zed palettes: Ayu from zed-industries/zed (assets/themes/ayu/ayu.json),
// Nightfox's opaque variants from cange/nightfox.zed (themes/nvim-nightfox.json).
// Each color's comment names its upstream key.
pub const AYU_DARK: Theme = Theme::from_palette(
    "ayu-dark",
    "Ayu Dark",
    Appearance::Dark,
    Palette {
        bg: hex(0x0d_1016),        // editor.background
        fg: hex(0xbf_bdb6),        // terminal.foreground
        surface: hex(0x1f_2127),   // surface.background
        raised: hex(0x2d_2f34),    // element.hover
        selection: hex(0x3e_4043), // element.selected
        border: hex(0x3f_4043),    // border
        muted: hex(0x8a_8986),     // text.muted
        red: hex(0xef_7177),       // terminal.ansi.red
        green: hex(0xaa_d84c),     // terminal.ansi.green
        yellow: hex(0xfe_b454),    // terminal.ansi.yellow
        blue: hex(0x5a_c1fe),      // terminal.ansi.blue
        // Ayu's terminal magenta is blue; use its purple constant color.
        magenta: hex(0xd2_a6ff), // syntax.constant
        cyan: hex(0x95_e5cb),    // terminal.ansi.cyan
    },
);

pub const AYU_LIGHT: Theme = Theme::from_palette(
    "ayu-light",
    "Ayu Light",
    Appearance::Light,
    Palette {
        bg: hex(0xfc_fcfc),        // editor.background
        fg: hex(0x5c_6166),        // terminal.foreground
        surface: hex(0xec_eced),   // surface.background
        raised: hex(0xdf_e0e1),    // element.hover
        selection: hex(0xcf_d0d2), // element.selected
        border: hex(0xcf_d1d2),    // border
        muted: hex(0x8b_8e92),     // text.muted
        red: hex(0xef_7271),       // terminal.ansi.red
        green: hex(0x85_b304),     // terminal.ansi.green
        yellow: hex(0xf1_ad49),    // terminal.ansi.yellow
        blue: hex(0x3b_9ee5),      // terminal.ansi.blue
        magenta: hex(0xa3_7acc),   // syntax.constant (see AYU_DARK)
        cyan: hex(0x4d_bf99),      // terminal.ansi.cyan
    },
);

pub const DAWNFOX: Theme = Theme::from_palette(
    "dawnfox",
    "Dawnfox - opaque",
    Appearance::Light,
    Palette {
        bg: hex(0xfa_f4ed),        // editor.background
        fg: hex(0x57_5279),        // terminal.foreground
        surface: hex(0xeb_e5df),   // surface.background
        raised: hex(0xd0_d8d8),    // editor.active_line.background
        selection: hex(0xb8_cece), // search.match_background
        border: hex(0xbd_bfc9),    // border.variant
        muted: hex(0x98_93a5),     // syntax.comment
        red: hex(0xb4_637a),       // terminal.ansi.red
        green: hex(0x61_8774),     // terminal.ansi.green
        yellow: hex(0xea_9d34),    // terminal.ansi.yellow
        blue: hex(0x28_6983),      // terminal.ansi.blue
        magenta: hex(0x90_7aa9),   // terminal.ansi.magenta
        cyan: hex(0x56_949f),      // terminal.ansi.cyan
    },
);

pub const DUSKFOX: Theme = Theme::from_palette(
    "duskfox",
    "Duskfox - opaque",
    Appearance::Dark,
    Palette {
        bg: hex(0x23_2136),        // editor.background
        fg: hex(0xe0_def4),        // terminal.foreground
        surface: hex(0x19_1726),   // surface.background
        raised: hex(0x43_3c59),    // editor.active_line.background
        selection: hex(0x63_577d), // search.match_background
        border: hex(0x4b_4673),    // border.variant
        muted: hex(0x81_7c9c),     // syntax.comment
        red: hex(0xeb_6f92),       // terminal.ansi.red
        green: hex(0xa3_be8c),     // terminal.ansi.green
        yellow: hex(0xf6_c177),    // terminal.ansi.yellow
        blue: hex(0x56_9fba),      // terminal.ansi.blue
        magenta: hex(0xc4_a7e7),   // terminal.ansi.magenta
        cyan: hex(0x9c_cfd8),      // terminal.ansi.cyan
    },
);

pub const NIGHTFOX: Theme = Theme::from_palette(
    "nightfox",
    "Nightfox - opaque",
    Appearance::Dark,
    Palette {
        bg: hex(0x19_2330),        // editor.background
        fg: hex(0xcd_cecf),        // terminal.foreground
        surface: hex(0x13_1a24),   // surface.background
        raised: hex(0x2b_3b51),    // editor.active_line.background
        selection: hex(0x3c_5372), // search.match_background
        border: hex(0x39_506d),    // border.variant
        muted: hex(0x73_8091),     // syntax.comment
        red: hex(0xc9_4f6d),       // terminal.ansi.red
        green: hex(0x81_b29a),     // terminal.ansi.green
        yellow: hex(0xdb_c074),    // terminal.ansi.yellow
        blue: hex(0x71_9cd6),      // terminal.ansi.blue
        magenta: hex(0x9d_79d6),   // terminal.ansi.magenta
        cyan: hex(0x63_cdcf),      // terminal.ansi.cyan
    },
);

/// Every shipped theme, default first, in listing and autocomplete order.
pub const THEMES: &[Theme] = &[CUPEL, AYU_DARK, AYU_LIGHT, DAWNFOX, DUSKFOX, NIGHTFOX];

/// Find a shipped theme by its exact id.
#[must_use]
pub fn find(id: &str) -> Option<&'static Theme> {
    THEMES.iter().find(|theme| theme.id == id)
}

/// Resolve a settings.json theme id. No value means the default; an unknown
/// id returns a warning so callers can fall back without blocking startup.
pub fn from_settings(id: Option<&str>) -> Result<&'static Theme, String> {
    let Some(id) = id else {
        return Ok(&CUPEL);
    };
    find(id).ok_or_else(|| {
        format!(
            "warning: unknown theme {id:?} in settings.json - using cupel (/themes lists the themes)"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_theme_is_found_by_its_id() {
        for theme in THEMES {
            assert_eq!(find(theme.id).map(|found| found.name), Some(theme.name));
            assert!(!theme.id.is_empty());
            assert!(
                theme.id.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
                "{}",
                theme.id
            );
        }
        let mut ids: Vec<&str> = THEMES.iter().map(|theme| theme.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), THEMES.len());
        assert!(find("dracula").is_none());
        assert!(find("Nightfox").is_none(), "ids are exact");
    }

    #[test]
    fn cupel_comes_first_then_two_light_and_three_dark_themes() {
        assert_eq!(THEMES[0].id, "cupel");
        assert_eq!(THEMES[0].appearance, Appearance::Terminal);
        assert_eq!(CUPEL.base, Style::new());
        for (id, appearance) in [
            ("ayu-dark", Appearance::Dark),
            ("ayu-light", Appearance::Light),
            ("dawnfox", Appearance::Light),
            ("duskfox", Appearance::Dark),
            ("nightfox", Appearance::Dark),
        ] {
            assert_eq!(
                find(id).map(|theme| theme.appearance),
                Some(appearance),
                "{id}"
            );
        }
    }

    #[test]
    fn settings_name_the_theme_and_a_bad_name_falls_back_with_a_warning() {
        assert_eq!(from_settings(None).map(|theme| theme.id), Ok("cupel"));
        assert_eq!(
            from_settings(Some("ayu-light")).map(|theme| theme.id),
            Ok("ayu-light")
        );
        let warning = from_settings(Some("dracula")).unwrap_err();
        assert!(warning.contains("\"dracula\""), "{warning}");
        assert!(warning.contains("using cupel"), "{warning}");
    }

    #[test]
    fn hex_splits_the_channels() {
        assert_eq!(hex(0x12_3456), Color::Rgb(0x12, 0x34, 0x56));
    }

    /// WCAG contrast ratio: 4.5 is the body-text minimum.
    fn contrast(first: Option<Color>, second: Option<Color>) -> f64 {
        let luminance = |color: Option<Color>| {
            let Some(Color::Rgb(red, green, blue)) = color else {
                panic!("palette themes paint RGB colors, got {color:?}");
            };
            let linear = |channel: u8| {
                let value = f64::from(channel) / 255.0;
                if value <= 0.039_28 {
                    value / 12.92
                } else {
                    ((value + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * linear(red) + 0.7152 * linear(green) + 0.0722 * linear(blue)
        };
        let (first, second) = (luminance(first), luminance(second));
        (first.max(second) + 0.05) / (first.min(second) + 0.05)
    }

    #[test]
    fn palette_text_is_readable_on_every_surface() {
        for theme in THEMES
            .iter()
            .filter(|t| t.appearance != Appearance::Terminal)
        {
            for (surface, ratio) in [
                ("background", contrast(theme.base.fg, theme.base.bg)),
                ("user message", contrast(theme.task.fg, theme.task.bg)),
                (
                    "code block",
                    contrast(theme.base.fg, theme.md_code_block.bg),
                ),
            ] {
                assert!(ratio >= 4.5, "{}: {surface} contrast {ratio:.2}", theme.id);
            }
        }
    }
}
