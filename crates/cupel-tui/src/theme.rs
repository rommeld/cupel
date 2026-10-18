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
//! [`CUPEL`] is the built-in default, using the terminal's own colors. Other
//! themes come from JSON files in `~/.cupel/themes` (see [`Theme::from_file`]).
//! [`registry`] hands them out as `Arc<Theme>`: shared, reference-counted values
//! that the sessions, the App and the event loop can all hold at once.
//!
//! CUPEL is a true `const`: ratatui`s `Style::new()`, `fg`, `bg`, and `add_modifier`
//! are const fns. Note the chained `add_modifier` calls where two modifiers combine.
//! The `|` operator (BitOr) is not a const fn, so `Modifier::DIM | Modifier::ITALIC`
//! would not compile here.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;

use cupel_coding_agent::themes::{FileAppearance, Rgb, ThemeFile};
use ratatui::style::{Color, Modifier, Style};

/// Every style the TUI paints, with, one field per role. A field names what the style
/// is for (`answer`), not how it looks (`magenta`), so each theme can pick its own
/// look for the same role.
#[derive(Debug, Clone)]
pub struct Theme {
    /// The lowercase id used by `/themes` and settings.json. The built-in theme
    /// borrows a string literal; a theme built at runtime owns its string.
    pub id: Cow<'static, str>,
    /// The name shown by `/themes`.
    pub name: Cow<'static, str>,
    pub appearance: Appearance,
    /// The file a user theme was read from; `None` for `CUPEL`.
    /// `/themes` shows it so the file can be found and edited.
    pub file: Option<PathBuf>,
    /// Painted over every cell before other styles. Empty in `CUPEL`,
    /// which keeps the terminal's own foreground and background.
    pub base: Style,
    /// User messages in the conversation.
    pub task: Style,
    /// The open `ask_user` question: bold, in the busy input border's color.
    pub question: Style,
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
    // win, unset fields keep the base. In CUPEL only code and links set a color, so a
    // heading in an Answer cell stays in the answer color. Palette themes also color
    // headings, bold, and italic, the way Zed's editor does.
    /// H1/H2: bold + underlined (H3-H6 get the same style without the underline).
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
    id: Cow::Borrowed("cupel"),
    name: Cow::Borrowed("Cupel"),
    appearance: Appearance::Terminal,
    file: None,
    base: Style::new(),
    // White on gray surface (xtrem-256 index 240, #585858).
    task: Style::new().fg(Color::White).bg(Color::Indexed(240)),
    question: Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
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

/// The 15 colors from which [`Theme::from_palette`] builds every style role.
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
    /// Final answer.
    pub magenta: Color,
    /// Tools, inline code, list bullets, popup rows.
    pub cyan: Color,
    // Markdown accents: the colors Zed's editor gives these elements.
    /// Headings (Zed's `syntax.title`).
    pub heading: Color,
    /// Bold and italic text (Zed's `syntax.emphasis`).
    pub emphasis: Color,
    /// Link text (Zed's `syntax.link_text`).
    pub link: Color,
}

impl Theme {
    /// Build an opaque theme with `CUPEL`'s hierarchy in a palette's colors.
    /// Headings, bold, italic, and links get the palette's Markdown accents.
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
            magenta,
            cyan,
            heading,
            emphasis,
            link,
        } = palette;
        Self {
            id: Cow::Borrowed(id),
            name: Cow::Borrowed(name),
            appearance,
            file: None,
            base: Style::new().fg(fg).bg(bg),
            task: Style::new().fg(fg).bg(raised),
            question: Style::new().fg(yellow).add_modifier(Modifier::BOLD),
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
                .fg(heading)
                .add_modifier(Modifier::BOLD)
                .add_modifier(Modifier::UNDERLINED),
            md_bold: Style::new().fg(emphasis).add_modifier(Modifier::BOLD),
            md_italic: Style::new().fg(emphasis).add_modifier(Modifier::ITALIC),
            md_strike: Style::new().add_modifier(Modifier::CROSSED_OUT),
            md_code: Style::new().fg(cyan),
            md_code_block: Style::new().bg(surface),
            md_quote: Style::new()
                .add_modifier(Modifier::DIM)
                .add_modifier(Modifier::ITALIC),
            md_bullet: Style::new().fg(cyan),
            md_link: Style::new().fg(link).add_modifier(Modifier::UNDERLINED),
            md_link_url: Style::new().add_modifier(Modifier::DIM),
            md_rule: Style::new().add_modifier(Modifier::DIM),
            md_table_header: Style::new().add_modifier(Modifier::BOLD),
        }
    }

    /// A theme from a file in `~/.cupel/themes`: palette colors mapped to
    /// style roles, with the file's id, name, and path.
    #[must_use]
    pub fn from_file(file: &ThemeFile) -> Self {
        let colors = file.palette;
        let palette = Palette {
            bg: color(colors.bg),
            fg: color(colors.fg),
            surface: color(colors.surface),
            raised: color(colors.raised),
            selection: color(colors.selection),
            border: color(colors.border),
            muted: color(colors.muted),
            red: color(colors.red),
            green: color(colors.green),
            yellow: color(colors.yellow),
            magenta: color(colors.magenta),
            cyan: color(colors.cyan),
            heading: color(colors.heading),
            emphasis: color(colors.emphasis),
            link: color(colors.link),
        };
        let appearance = match file.appearance {
            FileAppearance::Dark => Appearance::Dark,
            FileAppearance::Light => Appearance::Light,
        };
        Self {
            id: Cow::Owned(file.id.clone()),
            name: Cow::Owned(file.name.clone()),
            file: Some(file.path.clone()),
            ..Self::from_palette("", "", appearance, palette)
        }
    }
}

/// A theme file's color as a ratatui color.
const fn color(Rgb(red, green, blue): Rgb) -> Color {
    Color::Rgb(red, green, blue)
}

/// Every theme `/themes` can switch to, shared without copying its styles.
/// `cupel` comes first at `themes[0]` (the default), then theme files in
/// file-name order. A file with an existing id replaces it in place.
#[must_use]
pub fn registry(files: &[ThemeFile]) -> Vec<Arc<Theme>> {
    let mut themes = vec![Arc::new(CUPEL)];
    for file in files {
        let theme = Arc::new(Theme::from_file(file));
        match themes.iter_mut().find(|slot| slot.id == theme.id) {
            Some(slot) => *slot = theme,
            None => themes.push(theme),
        }
    }
    themes
}

/// Find a theme by its exact id.
#[must_use]
pub fn find(themes: &[Arc<Theme>], id: &str) -> Option<Arc<Theme>> {
    themes.iter().find(|theme| theme.id == id).cloned()
}

/// Resolve a settings.json theme id. No value means the default; an unknown
/// id returns a warning so callers can fall back without blocking startup.
pub fn from_settings(themes: &[Arc<Theme>], id: Option<&str>) -> Result<Arc<Theme>, String> {
    let Some(id) = id else {
        return Ok(Arc::clone(&themes[0]));
    };
    find(themes, id).ok_or_else(|| {
        format!(
            "warning: unknown theme {id:?} in settings.json - using cupel (/themes lists the themes)"
        )
    })
}

/// Repository JSON examples, never the user's home, for deterministic tests.
#[cfg(test)]
pub(crate) fn example_themes() -> Vec<Arc<Theme>> {
    let home = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let (files, warnings) = cupel_coding_agent::themes::load_theme_files(Some(&home));
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(files.len(), 5);
    registry(&files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_theme_is_found_by_its_id() {
        let themes = example_themes();
        for theme in &themes {
            assert_eq!(
                find(&themes, &theme.id).map(|found| found.name.clone()),
                Some(theme.name.clone())
            );
            assert!(!theme.id.is_empty());
            assert!(
                theme.id.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
                "{}",
                theme.id
            );
        }
        let mut ids: Vec<&str> = themes.iter().map(|theme| theme.id.as_ref()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), themes.len());
        assert!(find(&themes, "dracula").is_none());
        assert!(find(&themes, "Nightfox").is_none(), "ids are exact");
    }

    #[test]
    fn cupel_is_the_only_builtin_and_remains_the_default() {
        let builtins = registry(&[]);
        assert_eq!(builtins.len(), 1);
        assert_eq!(builtins[0].id, "cupel");
        assert_eq!(builtins[0].appearance, Appearance::Terminal);
        assert_eq!(from_settings(&builtins, None).unwrap().id, "cupel");
        assert!(from_settings(&builtins, Some("nightfox")).is_err());
        assert_eq!(CUPEL.base, Style::new());
        let themes = example_themes();
        assert_eq!(from_settings(&themes, None).unwrap().id, "cupel");
        for (id, appearance) in [
            ("ayu-dark", Appearance::Dark),
            ("ayu-light", Appearance::Light),
            ("dawnfox", Appearance::Light),
            ("duskfox", Appearance::Dark),
            ("nightfox", Appearance::Dark),
        ] {
            assert_eq!(
                find(&themes, id).map(|theme| theme.appearance),
                Some(appearance),
                "{id}"
            );
        }
    }

    #[test]
    fn settings_name_the_theme_and_a_bad_name_falls_back_with_a_warning() {
        let themes = example_themes();
        let id = |theme: Arc<Theme>| theme.id.to_string();
        assert_eq!(
            from_settings(&themes, None).map(id),
            Ok("cupel".to_string())
        );
        assert_eq!(
            from_settings(&themes, Some("ayu-light")).map(id),
            Ok("ayu-light".to_string())
        );
        let warning = from_settings(&themes, Some("dracula")).unwrap_err();
        assert!(warning.contains("\"dracula\""), "{warning}");
        assert!(warning.contains("using cupel"), "{warning}");
    }

    #[test]
    fn theme_files_join_the_registry_in_file_order() {
        use cupel_coding_agent::themes::FilePalette;

        let gray = Rgb(0x28, 0x28, 0x28);
        let light = Rgb(0xeb, 0xdb, 0xb2);
        let palette = FilePalette {
            bg: gray,
            fg: light,
            surface: gray,
            raised: gray,
            selection: gray,
            border: gray,
            muted: light,
            red: light,
            green: light,
            yellow: light,
            magenta: light,
            cyan: light,
            heading: Rgb(0xb8, 0xbb, 0x26),
            emphasis: light,
            link: light,
        };
        let file = |id: &str, name: &str| ThemeFile {
            id: id.to_string(),
            name: name.to_string(),
            appearance: FileAppearance::Dark,
            palette,
            path: PathBuf::from(format!("/home/u/.cupel/themes/{id}.json")),
        };
        let themes = registry(&[
            file("gruvbox-dark", "Gruvbox Dark"),
            file("nightfox", "My Nightfox"),
        ]);

        // Only CUPEL is built in; both file themes are appended in input order.
        let ids: Vec<&str> = themes.iter().map(|theme| theme.id.as_ref()).collect();
        assert_eq!(ids, ["cupel", "gruvbox-dark", "nightfox"]);
        assert_eq!(themes[2].name, "My Nightfox");
        let gruvbox = &themes[1];
        assert_eq!(gruvbox.name, "Gruvbox Dark");
        assert_eq!(gruvbox.appearance, Appearance::Dark);
        assert_eq!(gruvbox.base.bg, Some(Color::Rgb(0x28, 0x28, 0x28)));
        assert_eq!(gruvbox.md_heading.fg, Some(Color::Rgb(0xb8, 0xbb, 0x26)));
        assert_eq!(
            gruvbox.file,
            Some(PathBuf::from("/home/u/.cupel/themes/gruvbox-dark.json"))
        );
        assert_eq!(themes[0].file, None);
    }

    #[test]
    fn palette_markdown_uses_zeds_editor_colors() {
        // Nightfox's values from Zed's theme file: text, syntax.title,
        // syntax.emphasis, syntax.link_text.
        let nightfox = find(&example_themes(), "nightfox").unwrap();
        assert_eq!(nightfox.base.fg, Some(Color::Rgb(0xd6, 0xd6, 0xd7)));
        assert_eq!(nightfox.md_heading.fg, Some(Color::Rgb(0x86, 0xab, 0xdc)));
        assert_eq!(nightfox.md_bold.fg, Some(Color::Rgb(0xc9, 0x4f, 0x6d)));
        assert_eq!(nightfox.md_italic.fg, Some(Color::Rgb(0xc9, 0x4f, 0x6d)));
        assert_eq!(nightfox.md_link.fg, Some(Color::Rgb(0x86, 0xab, 0xdc)));
        // CUPEL sets no color here, so headings and emphasis keep the cell's.
        assert_eq!(CUPEL.md_heading.fg, None);
        assert_eq!(CUPEL.md_bold.fg, None);
        assert_eq!(CUPEL.md_italic.fg, None);
    }

    #[test]
    fn file_colors_keep_their_channels() {
        assert_eq!(color(Rgb(0x12, 0x34, 0x56)), Color::Rgb(0x12, 0x34, 0x56));
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
        for theme in example_themes()
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
