//! Persistent settings definition.
//!
//! Saves normalize formatting (pretty-printed, keys sorted): values and
//! unknown fields are preserved. Two concurrent cupel instances saving
//! at once each write a complete valid file; the last rename wins and
//! one update can be lost - same accepted caveat as concurrent
//! `--resume` in session.rs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// BTreeMap (not HashMap) so re-serialization is determenstic:
    /// HashMap iteration order is randomized per process.
    #[serde(default)]
    pub providers: BTreeMap<String, String>,
    /// Loop-killer section; None = not configured. Opt-in by decision: an
    /// absent section changes nothing about existing behavior.
    /// skip_serializing_if: a key save must not inject "loopKiller": null
    /// into a file whose author never wrote that key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_killer: Option<LoopKillerSettings>,
    /// Every unknown top-level field, round-tripped through save.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Loop-killer configuration: how many times the SAME tool call (same
/// name, identical arguments) may run back-to-back before the repeat is
/// blocked and the model is steered onto a new track (see
/// `crate::loop_killer)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoopKillerSettings {
    /// 0 = explicitly disabled (same as leaving the section out).
    pub max_repeats: u32,
}

impl Settings {
    /// The stored key for a provider, if any.
    #[must_use]
    pub fn api_key(&self, provider: &str) -> Option<&str> {
        self.providers.get(provider).map(String::as_str)
    }
    /// The effective repeat allowance; None = loop killer off. Opt-in by
    /// decision: no loopKiller section (or maxRepeats 0) means `cupel`
    /// behaves exactly as before this feature existed.
    #[must_use]
    pub fn loop_killer_max_repeats(&self) -> Option<u32> {
        let configured = self.loop_killer?.max_repeats;
        (configured > 0).then_some(configured)
    }
    /// Merge home and project into the runtime view, per FIELD:
    /// - providers: HOME ONLY. A project settings.json must never supply
    /// credentials (warn_project_settings already told the user why);
    /// the project's map is deliverately not touched at all.
    /// - loopKiller: project overrides home when present (the usual
    /// later-wins layering, mirroring models.json).
    /// extra: the home copy rides along - it exists for the SAVE
    /// round-trip, which only ever writes the home file.
    #[must_use]
    pub fn layered(home: Settings, project: Settings) -> Settings {
        Settings {
            providers: home.providers,
            loop_killer: project.loop_killer.or(home.loop_killer),
            extra: home.extra,
        }
    }
}

/// Manuel Debug that prints only, never key values - `{:?}` output
/// reaces logs, panics, and failed-assertion messages, and secrets
/// must not. (This also makes assert_eq! failures in tests
/// safe to paste anywhere.)
impl core::fmt::Debug for Settings {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Settings")
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
            .field("extra", &self.extra.keys().collect::<Vec<_>>())
            .field("loopKiller", &self.loop_killer)
            .finish_non_exhaustive()
    }
}

/// `~/.cupel/settings.json`.
#[must_use]
pub fn settings_path(home: &Path) -> PathBuf {
    home.join("settings.json")
}

/// `<cwd>/.cupel/settings.json`.
#[must_use]
pub fn project_settings_path(cwd: &Path) -> PathBuf {
    cwd.join(".cupel/settings.json")
}

/// Parse the settings file. A missing file is simply empty defaults; a
/// MALFORMED file is an error the caller must surface - a config the user
/// wrote by hand deserves a visible failure (same tiers as
/// `models::load_models_file`).
pub fn load_settings(path: &Path) -> Result<Settings, String> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Settings::default()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    serde_json::from_str(&content).map_err(|e| format!("{} is not valid: {e}", path.display()))
}

#[must_use]
pub fn load_home_settings(home: Option<&Path>) -> Settings {
    let Some(home) = home else {
        return Settings::default();
    };
    match load_settings(&settings_path(home)) {
        Ok(settings) => settings,
        Err(e) => {
            eprintln!(
                "warning: ignoring settings file: {e} (fix the JSON syntax, e.g. remove trailing commas)"
            );
            Settings::default()
        }
    }
}

/// The project layer (`<cwd>/.cupel/settings.json`), with the same
/// warn-and-default policy as the home layer: a malformed hand-written
/// file is announced and skipped, never fatal. Secrets are NOT filtered
/// here - `layered`simply never reads this layer`s providers map, so
/// filtering has nothing to do (security by construction beats secruity
/// by inspection).
#[must_use]
pub fn load_project_settings(cwd: &Path) -> Settings {
    match load_settings(&project_settings_path(cwd)) {
        Ok(settings) => settings,
        Err(e) => {
            eprintln!("warning: ignoring project settings file: {e}");
            Settings::default()
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    #[error("no cupel home resolvable - set CUPEL_HOME or home directory")]
    NoHome,
    #[error("{} is not valid JSON ({reason}) - fix or remove, then retry", path.display())]
    Malformed { path: PathBuf, reason: String },
    #[error("cannot write {}: {reason}", path.display())]
    Io { path: PathBuf, reason: String },
}

/// Read-modify-write ONE provider key into `<home>/settings.json`,
/// creating the home directory if needed. Returns the path written (for
/// the TUI notice).
///
/// Design points:
/// - The file is RE-READ here instead of trusting any in-memory copy:
/// hand edits made while `cupel` runs are preserved, and a malformed file
/// is detected and refused rather than silently overwritten.
/// - The new content goes to a temp file in the SAME directory, then
/// renames over the target. rename is atomic only within one filesystem
/// (a temp_dir on another mount would fail with EXDEV), and atomicity
/// means a reader sees either the old or the new complete file, never a
/// torn one.
/// - The temp file is created 0600 on Unix BEFORE any secret byte lands
/// on disk; rename carries that mode to the final path, tightening even
/// a pre-existing 0644 file.
pub fn save_provider_key(
    home: Option<&Path>,
    provider: &str,
    key: &str,
) -> Result<PathBuf, SaveError> {
    use std::io::Write as _;

    let home = home.ok_or(SaveError::NoHome)?;
    let path = settings_path(home);

    // Fresh from disk, not from memory. NotFound is the one io error
    // that is NOT an error here: first save ever.
    let mut settings = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str::<Settings>(&text).map_err(|e| SaveError::Malformed {
            path: path.clone(),
            reason: e.to_string(),
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Settings::default(),
        Err(e) => {
            return Err(SaveError::Io {
                path: path.clone(),
                reason: e.to_string(),
            });
        }
    };

    settings
        .providers
        .insert(provider.to_string(), key.to_string());

    // The home dir may not exist yet (first run, fresh CUPEL_HOME). No
    // exists() pre-check: create_dir_all is idempotent, and attempting
    // unconditionally avoids a check-then-act race.
    std::fs::create_dir_all(home).map_err(|e| SaveError::Io {
        path: home.to_path_buf(),
        reason: e.to_string(),
    })?;

    // Pretty + trailing newline: this file is hand-edited; compact JSON
    // would punish the user on their next manual edit.
    let mut body = serde_json::to_string_pretty(&settings).map_err(|e| SaveError::Io {
        path: path.clone(),
        reason: e.to_string(),
    })?;
    body.push('\n');

    // Same-directory temp file. A stale tmp from a crashed save must not
    // male create_new fail forever, so clear it first (best effort).
    let tmp = home.join("settings.json.tmp");
    let _ = std::fs::remove_file(&tmp);

    let mut options = std::fs::OpenOptions::new();
    // create_new (not create+truncate): a file or symlink squatting on the
    // tmp path fails loudly instead of being followed.
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        // Owner-only BEFORE any secret byte lands on disk - the mode
        // applies at creation, which is exactly why a fresh tmp file (and
        // not chmod-after-write on the real file) is the safe order.
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp).map_err(|e| SaveError::Io {
        path: tmp.clone(),
        reason: e.to_string(),
    })?;

    // fsync BEFORE rename: without it a power loss can publish an empty
    // tmp file over a good settings.json - rename orders metadata, not
    // data. flush would only empty userspace buffers; sync_all reaches
    // the disk.
    let written = file
        .write_all(body.as_bytes())
        .and_then(|()| file.sync_all());
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(SaveError::Io {
            path: tmp,
            reason: e.to_string(),
        });
    }
    // Close before rename.
    drop(file);

    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(SaveError::Io {
            path: path.clone(),
            reason: e.to_string(),
        });
    }

    Ok(path)
}

/// True when the file exists, parses, and defines a top-level "providers"
/// key. A pure predicate so the warning policy is testable separately.
/// Parsed as a generic Value (not Settings): the QUESTION here is only
/// "did someone put providers ina project file?", not schema validity -
/// and a malformed files defines nothing, so it stays silent rather than
/// training users to think project settings are honored.
fn project_settings_define_providers(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .is_some_and(|value| value.get("providers").is_some())
}

/// Keys in a PROJECT settings file are one `git add` away from a leaked
/// secret, so they are never honored - only warned about. A project file
/// WITHOUT a providers key stays silent.
pub fn warn_project_settings(cwd: &Path) {
    let path = project_settings_path(cwd);
    if project_settings_define_providers(&path) {
        eprintln!(
            "warning: provider keys in {} are ignored - API keys belong in \
            ~/.cupel/settings.json only; remove them before they get commited",
            path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cupel-settings-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_file_loads_defaults() {
        let root = temp_root("missing");
        let settings = load_settings(&root.join("settings.jons")).unwrap();
        assert!(settings.providers.is_empty());
        assert!(load_home_settings(None).providers.is_empty());
    }

    #[test]
    fn valid_file_parses_providers() {
        let root = temp_root("valid");
        let path = root.join("settings.json");
        std::fs::write(
            &path,
            r#"{"providers": {"anthropic": "sk-a", "fireworks": "fw-b"}}"#,
        )
        .unwrap();
        let settings = load_settings(&path).unwrap();
        assert_eq!(settings.api_key("anthropic"), Some("sk-a"));
        assert_eq!(settings.api_key("fireworks"), Some("fw-b"));
        assert_eq!(settings.api_key("unknown"), None);
    }

    #[test]
    fn malformed_files_are_errors_and_the_wrapper_defaults() {
        let root = temp_root("malformed");
        let path = root.join("settings.jons");
        for bad in ["{not json", "[]", r#"{"providers": {"anthropic": 42}}"#] {
            std::fs::write(&path, bad).unwrap();
            assert!(load_settings(&path).is_err(), "should reject: {bad}");
        }
        assert!(load_home_settings(Some(&root)).providers.is_empty());
    }

    #[test]
    fn unknown_fields_round_trip() {
        let input = r#"{"providers": {"anthropic": "sk-a"}, "editor": {"theme": "dark"}}"#;
        let parsed: Settings = serde_json::from_str(input).unwrap();
        assert_eq!(parsed.extra["editor"]["theme"], serde_json::json!("dark"));

        let serialized = serde_json::to_string_pretty(&parsed).unwrap();
        let reparsed: Settings = serde_json::from_str(&serialized).unwrap();
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn serialized_provider_order_is_deterministic() {
        let mut settings = Settings::default();
        settings.providers.insert("zeta".into(), "z".into());
        settings.providers.insert("alpha".into(), "a".into());
        let text = serde_json::to_string_pretty(&settings).unwrap();
        let (alpha, zeta) = (
            text.find("alpha").expect("alpha present"),
            text.find("zeta").expect("zeta present"),
        );
        assert!(alpha < zeta, "BTreeMap must sort keys:\n{text}");
    }

    #[test]
    fn debug_never_prints_key_values() {
        let mut settings = Settings::default();
        settings.providers.insert("anthropic".into(), "sk-a".into());
        let renderd = format!("{settings:?}");
        assert!(renderd.contains("anthropic"), "{renderd}");
        assert!(!renderd.contains("sk-a"), "key value leaked: {renderd}");
    }

    #[test]
    fn save_creates_the_home_dir_and_file() {
        let root = temp_root("save-create");
        let home = root.join("fresh-home"); // does not exist yet
        let path = save_provider_key(Some(&home), "anthropic", "sk-a").unwrap();
        assert_eq!(path, home.join("settings.json"));
        assert_eq!(
            load_settings(&path).unwrap().api_key("anthropic"),
            Some("sk-a")
        );
    }

    #[test]
    fn save_preserves_other_providers_and_unknown_fields() {
        let home = temp_root("save-preserve");
        // Compact, oddly-spaced hand-written file: values must survive
        // (formatting is normalized, and that is documented).
        std::fs::write(
            home.join("settings.json"),
            r#"{"providers":{"fireworks":"fw-1"},"editor":"vim"}"#,
        )
        .unwrap();
        save_provider_key(Some(&home), "anthropic", "sk-a").unwrap();
        let settings = load_settings(&home.join("settings.json")).unwrap();
        assert_eq!(settings.api_key("fireworks"), Some("fw-1"));
        assert_eq!(settings.api_key("anthropic"), Some("sk-a"));
        assert_eq!(settings.extra["editor"], serde_json::json!("vim"));
    }

    #[test]
    fn save_overwrites_the_same_provider() {
        let home = temp_root("save-overwrite");
        save_provider_key(Some(&home), "anthropic", "old").unwrap();
        save_provider_key(Some(&home), "anthropic", "new").unwrap();
        let settings = load_settings(&home.join("settings.json")).unwrap();
        assert_eq!(settings.providers.len(), 1);
        assert_eq!(settings.api_key("anthropic"), Some("new"));
    }

    #[test]
    fn save_refuses_a_malformed_file() {
        let home = temp_root("save-refuse");
        let path = home.join("settings.json");
        std::fs::write(&path, "{broken").unwrap();
        let err = save_provider_key(Some(&home), "anthropic", "sk-a").unwrap_err();
        assert!(matches!(err, SaveError::Malformed { .. }), "{err}");
        // The user's bytes are untouched, and no tmp litter remains.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{broken");
        assert!(!home.join("settings.json.tmp").exists());
    }

    #[test]
    fn save_without_a_home_is_no_home() {
        let err = save_provider_key(None, "anthropic", "sk-a").unwrap_err();
        assert!(matches!(err, SaveError::NoHome));
    }

    #[test]
    fn save_replaces_a_stale_tmp_file() {
        let home = temp_root("save-stale-tmp");
        std::fs::write(home.join("settings.json.tmp"), "garbage from a crash").unwrap();
        save_provider_key(Some(&home), "anthropic", "sk-a").unwrap();
        assert!(!home.join("settings.json.tmp").exists());
        assert_eq!(
            load_settings(&home.join("settings.json"))
                .unwrap()
                .api_key("anthropic"),
            Some("sk-a")
        );
    }

    #[test]
    fn saved_output_is_pretty_and_newline_terminated() {
        let home = temp_root("save-pretty");
        save_provider_key(Some(&home), "anthropic", "sk-a").unwrap();
        let text = std::fs::read_to_string(home.join("settings.json")).unwrap();
        assert!(text.contains("\n  \"providers\""), "not pretty:\n{text}");
        assert!(text.ends_with('\n'));
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_is_owner_only_even_over_an_existing_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let home = temp_root("save-perms");
        let path = home.join("settings.json");
        // Pre-existing file with default (usually 0644) permissions: the
        // rename must carry the tmp file's 0600 over it - the file holds
        // secrets now.
        std::fs::write(&path, "{}").unwrap();
        save_provider_key(Some(&home), "anthropic", "sk-a").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {mode:o}");
    }

    #[test]
    fn project_settings_providers_detection() {
        let root = temp_root("project-guard");
        let path = root.join("settings.json");
        assert!(!project_settings_define_providers(&path), "missing file");
        std::fs::write(&path, r#"{"other": 1}"#).unwrap();
        assert!(
            !project_settings_define_providers(&path),
            "no providers key"
        );
        std::fs::write(&path, "{broken").unwrap();
        assert!(
            !project_settings_define_providers(&path),
            "malformed defines nothing"
        );
        std::fs::write(&path, r#"{"providers": {"anthropic": "leak"}}"#).unwrap();
        assert!(project_settings_define_providers(&path));
    }

    #[test]
    fn loop_killer_settings_parse_and_default_off() {
        let parsed: Settings =
            serde_json::from_str(r#"{"loopKiller": {"maxRepeats": 5}}"#).unwrap();
        assert_eq!(parsed.loop_killer_max_repeats(), Some(5));
        // A known field now - it must NOT fall into the extra map.
        assert!(parsed.extra.is_empty());

        // Opt-in: absent section = off, explicit 0 = off.
        assert_eq!(Settings::default().loop_killer_max_repeats(), None);
        let zero: Settings = serde_json::from_str(r#"{"loopKiller": {"maxRepeats": 0}}"#).unwrap();
        assert_eq!(zero.loop_killer_max_repeats(), None);
    }

    #[test]
    fn save_does_not_inject_a_null_loop_killer() {
        let home = temp_root("save-no-null");
        save_provider_key(Some(&home), "anthropic", "sk-a").unwrap();
        let text = std::fs::read_to_string(home.join("settings.json")).unwrap();
        assert!(
            !text.contains("loopKiller"),
            "skip_serializing_if failed:\n{text}"
        );
    }

    #[test]
    fn save_preserves_a_loop_killer_section() {
        let home = temp_root("save-keep-lk");
        std::fs::write(
            home.join("settings.json"),
            r#"{"loopKiller": {"maxRepeats": 4}, "providers": {}}"#,
        )
        .unwrap();
        save_provider_key(Some(&home), "anthropic", "sk-a").unwrap();
        let settings = load_settings(&home.join("settings.json")).unwrap();
        assert_eq!(settings.loop_killer_max_repeats(), Some(4));
        assert_eq!(settings.api_key("anthropic"), Some("sk-a"));
    }

    #[test]
    fn layered_project_overrides_loop_killer_but_never_supplies_keys() {
        let mut home = Settings::default();
        home.providers.insert("anthropic".into(), "sk-home".into());
        home.loop_killer = Some(LoopKillerSettings { max_repeats: 3 });
        let mut project = Settings::default();
        // A hostile or careless project file: both a key AND a config.
        project
            .providers
            .insert("anthropic".into(), "sk-LEAKED".into());
        project.loop_killer = Some(LoopKillerSettings { max_repeats: 7 });

        let merged = Settings::layered(home.clone(), project);
        assert_eq!(merged.loop_killer_max_repeats(), Some(7), "project wins");
        assert_eq!(
            merged.api_key("anthropic"),
            Some("sk-home"),
            "keys must stay home-only"
        );

        // Without a project section the home value stands.
        let merged = Settings::layered(home, Settings::default());
        assert_eq!(merged.loop_killer_max_repeats(), Some(3));
    }

    #[test]
    fn project_settings_load_warns_and_defaults_like_home() {
        let root = temp_root("project-load");
        std::fs::create_dir_all(root.join(".cupel")).unwrap();
        assert_eq!(
            load_project_settings(&root),
            Settings::default(),
            "missing file = defaults"
        );
        std::fs::write(root.join(".cupel/settings.json"), "{broken").unwrap();
        assert_eq!(
            load_project_settings(&root),
            Settings::default(),
            "malformed = warn + defaults"
        );
        std::fs::write(
            root.join(".cupel/settings.json"),
            r#"{"loopKiller": {"maxRepeats": 2}}"#,
        )
        .unwrap();
        assert_eq!(
            load_project_settings(&root).loop_killer_max_repeats(),
            Some(2)
        );
    }
}
