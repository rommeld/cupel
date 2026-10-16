//! Theme files: `<cupel home>/themes/<id>.json`, one color theme each.
//!
//! This module reads and checks them as plain data, colors as three bytes;
//! cupel-tui turns them into styles. It knows nothing about ratatui, so this
//! crate stays free of UI dependencies. The file name is the theme's id, the
//! way a prompt template's file name is its command.
//!
//! A broken file costs only itself: it is skipped with a warning that names
//! the file and the problem, and every other theme still loads.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// A color as a theme file writes it: `"#rrggbb"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct Rgb(pub u8, pub u8, pub u8);

impl core::str::FromStr for Rgb {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let digits = text
            .strip_prefix('#')
            .filter(|digits| digits.len() == 6 && digits.chars().all(|c| c.is_ascii_hexdigit()))
            .ok_or_else(|| format!("{text:?} is not a color, expected \"#rrggbb\""))?;
        let value = u32::from_str_radix(digits, 16).map_err(|error| error.to_string())?;
        Ok(Self((value >> 16) as u8, (value >> 8) as u8, value as u8))
    }
}

impl TryFrom<String> for Rgb {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        text.parse()
    }
}

/// The 15 colors of a theme file's `"palette"`, matching cupel-tui's `Palette`.
/// Every color is required; an unknown name is an error, not a silent typo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilePalette {
    pub bg: Rgb,
    pub fg: Rgb,
    pub surface: Rgb,
    pub raised: Rgb,
    pub selection: Rgb,
    pub border: Rgb,
    pub muted: Rgb,
    pub red: Rgb,
    pub green: Rgb,
    pub yellow: Rgb,
    pub magenta: Rgb,
    pub cyan: Rgb,
    pub heading: Rgb,
    pub emphasis: Rgb,
    pub link: Rgb,
}

/// Which background a theme file is made for. A file theme always paints
/// its own background, so only `"dark"` and `"light"` are supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileAppearance {
    Dark,
    Light,
}

/// The JSON object in a theme file, exactly as written.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ThemeJson {
    name: String,
    appearance: FileAppearance,
    palette: FilePalette,
}

/// One theme file that loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeFile {
    /// The file name without `.json`: what `/themes` and settings.json use.
    pub id: String,
    /// The name `/themes` shows.
    pub name: String,
    pub appearance: FileAppearance,
    pub palette: FilePalette,
    /// Where the file is, for `/themes` and for warnings.
    pub path: PathBuf,
}

/// `<home>/themes`.
#[must_use]
pub fn themes_dir(home: &Path) -> PathBuf {
    home.join("themes")
}

/// Read every `*.json` file in `<home>/themes`, sorted by file name. No home
/// or no folder means no themes and no warning. A file that doesn't load is
/// left out with a warning; a theme whose text is hard to read loads with a
/// warning.
#[must_use]
pub fn load_theme_files(home: Option<&Path>) -> (Vec<ThemeFile>, Vec<String>) {
    let mut themes = Vec::new();
    let mut warnings = Vec::new();
    let Some(home) = home else {
        return (themes, warnings);
    };
    let Ok(entries) = std::fs::read_dir(themes_dir(home)) else {
        return (themes, warnings);
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    // `read_dir` returns the files in no particular order.
    paths.sort();
    for path in paths {
        match load_theme_file(&path) {
            Ok(theme) => {
                warnings.extend(readability_warning(&theme));
                themes.push(theme);
            }
            Err(error) => warnings.push(format!(
                "warning: ignoring theme file {}: {error}",
                path.display()
            )),
        }
    }
    (themes, warnings)
}

/// One file: its id from the file name, its content from the JSON.
fn load_theme_file(path: &Path) -> Result<ThemeFile, String> {
    let id = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default()
        .to_string();
    let valid_id = !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !valid_id {
        return Err(
            "the file name is the theme's id: use lowercase letters, digits and dashes".to_string(),
        );
    }
    let text = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    let json: ThemeJson = serde_json::from_str(&text).map_err(|error| error.to_string())?;
    Ok(ThemeFile {
        id,
        name: json.name,
        appearance: json.appearance,
        palette: json.palette,
        path: path.to_path_buf(),
    })
}

/// WCAG contrast ratio of two colors: 1 (the same) up to 21 (black on white).
#[must_use]
pub fn contrast(first: Rgb, second: Rgb) -> f64 {
    let luminance = |Rgb(red, green, blue): Rgb| {
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

/// Below 4.5:1, the WCAG minimum for body text, on any text background.
/// Only a warning: the theme is the user's choice.
fn readability_warning(theme: &ThemeFile) -> Option<String> {
    let palette = theme.palette;
    [
        ("background", palette.bg),
        ("user message", palette.raised),
        ("code block", palette.surface),
    ]
    .into_iter()
    .map(|(surface, color)| (surface, contrast(palette.fg, color)))
    .find(|(_, ratio)| *ratio < 4.5)
    .map(|(surface, ratio)| {
        format!(
            "warning: theme file {}: text is hard to read on the {surface} (contrast {ratio:.1}:1, aim for 4.5:1)",
            theme.path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use crate::themes::{FileAppearance, Rgb, load_theme_files, themes_dir};
    use std::path::PathBuf;

    fn temp_home(name: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!("cupel-themes-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(themes_dir(&home)).unwrap();
        home
    }

    /// A valid theme file: Gruvbox Dark. Tests break it with `.replace(…)`.
    fn gruvbox() -> String {
        r##"{
            "name": "Gruvbox Dark",
            "appearance": "dark",
            "palette": {
                "bg": "#282828", "fg": "#ebdbb2", "surface": "#1d2021",
                "raised": "#3c3836", "selection": "#504945", "border": "#665c54",
                "muted": "#928374", "red": "#fb4934", "green": "#b8bb26",
                "yellow": "#fabd2f", "magenta": "#d3869b", "cyan": "#8ec07c",
                "heading": "#b8bb26", "emphasis": "#fe8019", "link": "#83a598"
            }
        }"##
        .to_string()
    }

    #[test]
    fn colors_are_hash_and_six_hex_digits() {
        assert_eq!("#192330".parse::<Rgb>(), Ok(Rgb(0x19, 0x23, 0x30)));
        assert_eq!("#ABCdef".parse::<Rgb>(), Ok(Rgb(0xab, 0xcd, 0xef)));
        for bad in ["192330", "#fff", "#1923300", "#19233g", "red", ""] {
            let error = bad.parse::<Rgb>().unwrap_err();
            assert!(error.contains("expected \"#rrggbb\""), "{bad}: {error}");
        }
    }

    #[test]
    fn theme_files_load_sorted_and_a_broken_one_costs_only_itself() {
        let home = temp_home("load");
        let dir = themes_dir(&home);
        std::fs::write(dir.join("zz-dark.json"), gruvbox()).unwrap();
        std::fs::write(dir.join("gruvbox-dark.json"), gruvbox()).unwrap();
        std::fs::write(dir.join("notes.txt"), "not a theme").unwrap();
        std::fs::write(
            dir.join("typo.json"),
            gruvbox().replace("heading", "hedding"),
        )
        .unwrap();
        std::fs::write(dir.join("Bad_Name.json"), gruvbox()).unwrap();
        std::fs::write(
            dir.join("bad-color.json"),
            gruvbox().replace("#fb4934", "#fb49"),
        )
        .unwrap();

        let (themes, warnings) = load_theme_files(Some(&home));
        let ids: Vec<&str> = themes.iter().map(|theme| theme.id.as_str()).collect();
        assert_eq!(ids, ["gruvbox-dark", "zz-dark"]);
        let theme = &themes[0];
        assert_eq!(theme.name, "Gruvbox Dark");
        assert_eq!(theme.appearance, FileAppearance::Dark);
        assert_eq!(theme.palette.bg, Rgb(0x28, 0x28, 0x28));
        assert_eq!(theme.path, dir.join("gruvbox-dark.json"));

        assert_eq!(warnings.len(), 3, "{warnings:#?}");
        for (file, reason) in [
            ("Bad_Name.json", "lowercase letters, digits and dashes"),
            ("bad-color.json", "\"#fb49\" is not a color"),
            ("typo.json", "unknown field `hedding`"),
        ] {
            assert!(
                warnings
                    .iter()
                    .any(|warning| warning.contains(file) && warning.contains(reason)),
                "{file}: {warnings:#?}"
            );
        }
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_missing_color_is_named() {
        let home = temp_home("missing");
        let short = gruvbox().replace(r##", "link": "#83a598""##, "");
        std::fs::write(themes_dir(&home).join("short.json"), short).unwrap();
        let (themes, warnings) = load_theme_files(Some(&home));
        assert!(themes.is_empty());
        assert!(warnings[0].contains("missing field `link`"), "{warnings:?}");
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn no_home_or_no_folder_means_no_themes() {
        assert_eq!(load_theme_files(None), (Vec::new(), Vec::new()));
        let home = temp_home("none");
        std::fs::remove_dir_all(&home).unwrap();
        assert_eq!(load_theme_files(Some(&home)), (Vec::new(), Vec::new()));
    }

    #[test]
    fn hard_to_read_text_loads_with_a_warning() {
        use crate::themes::contrast;

        let home = temp_home("contrast");
        let murky = gruvbox().replace("#ebdbb2", "#333333");
        std::fs::write(themes_dir(&home).join("murky.json"), murky).unwrap();
        let (themes, warnings) = load_theme_files(Some(&home));
        assert_eq!(themes.len(), 1);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("murky.json"), "{warnings:?}");
        assert!(
            warnings[0].contains("hard to read on the background"),
            "{warnings:?}"
        );
        assert!(contrast(Rgb(0, 0, 0), Rgb(255, 255, 255)) > 20.9);
        std::fs::remove_dir_all(home).unwrap();
    }
}
