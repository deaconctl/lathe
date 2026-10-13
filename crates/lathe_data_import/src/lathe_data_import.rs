//! One-time import of a user's data from stock Zed's folders into Lathe's.
//!
//! Lathe used to share Zed's config and data directories, which let the two
//! apps break each other: diverged database migrations, language servers
//! deleting each other's downloads, and a shared CLI socket on Linux. Lathe now
//! has its own directories, and on first launch this copies the user's data
//! over so nothing appears to change for them.
//!
//! Zed's folders are only read, with one deliberate exception: databases that
//! Lathe wrote its own migration rows into are repaired so stock Zed can open
//! them again (see [`repair_zed_database`]).

use anyhow::{Context as _, Result, anyhow};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

/// The Cargo binary keeps upstream's name so upstream's packaging scripts keep
/// working; see the assert in `crates/zed/src/main.rs`.
pub const EXPECTED_CARGO_BIN_NAME: &str = "zed";

/// Checked against `paths::APP_NAME` at compile time, so an upstream merge that
/// resets it to "Zed" fails the build instead of pointing Lathe at Zed's data.
pub const EXPECTED_APP_NAME: &str = "Lathe";

const LEGACY_APP_NAME: &str = "Zed";
const LEGACY_APP_NAME_LOWERCASE: &str = "zed";

const MARKER_FILE_NAME: &str = ".imported_from_zed";
const LOCK_FILE_NAME: &str = ".import.lock";
const TEMPORARY_SUFFIX: &str = "lathe-import-tmp";
const ZED_DATABASE_BACKUP_SUFFIX: &str = "before-lathe-repair";
const FAILED_IMPORT_BACKUP_SUFFIX: &str = "before-lathe-import";
const STABLE_REFRESH_BACKUP_SUFFIX: &str = "before-stable-import";
const STABLE_REFRESH_KEY: &str = "refresh_from_zed/stable";
const MAX_ATTEMPTS: u32 = 3;
const STALE_LOCK_AGE: Duration = Duration::from_secs(120);
const LOCK_WAIT: Duration = Duration::from_secs(5);

/// Database scopes stock Zed opens. Lathe-only channels (beta) are left alone:
/// Zed never reads them, and an older Lathe build may still be using them.
const ZED_DATABASE_SCOPES: &[&str] = &["stable", "preview", "nightly", "global"];

/// Database scopes owned by a Lathe channel. Older builds of that channel keep
/// using Zed's copy until they update, so only the channel's own import copies
/// or repairs it: copying earlier leaves Lathe a stale copy, and repairing
/// earlier makes the older build fail with "Migration changed".
const LATHE_CHANNEL_SCOPES: &[&str] = &["stable", "beta"];

/// Migrations Lathe added to domains it shares with upstream. Each entry is the
/// domain and a fragment of the migration's SQL. Upstream's own steps follow
/// Lathe's in the stored list, so removing these rows (and renumbering) gives
/// stock Zed back the list it expects. Keep in sync with
/// `crates/workspace/src/persistence.rs` and
/// `crates/agent_ui/src/thread_metadata_store.rs`.
const LATHE_ONLY_MIGRATIONS: &[(&str, &str)] = &[
    (
        "WorkspaceDb",
        "ALTER TABLE workspaces ADD COLUMN collab_account_id",
    ),
    (
        "ThreadMetadataDb",
        "ALTER TABLE sidebar_threads ADD COLUMN workspace_id",
    ),
];

/// Data-dir entries copied with a plain copy (or a clone, where the filesystem
/// makes that free).
const DATA_ENTRIES: &[&str] = &[
    "extensions/installed",
    "extensions/index.json",
    "prompts",
    "embeddings",
];

/// Downloads that are re-fetched on demand. They are only copied when the
/// filesystem can clone them for free; copying gigabytes at launch would be
/// slower than downloading what's needed later.
const CLONE_ONLY_ENTRIES: &[&str] = &[
    "extensions/work",
    "languages",
    "debug_adapters",
    "prettier",
    "external_agents",
    "copilot",
];

#[cfg(target_os = "windows")]
const LEGACY_WINDOWS_CREDENTIAL_PREFIX: &str = "zed:url=";
/// Must match `windows_credentials_target_name` in `crates/gpui_windows/src/util.rs`.
#[cfg(target_os = "windows")]
const WINDOWS_CREDENTIAL_PREFIX: &str = "lathe:url=";

/// What the import did, for logging once logging is up and for telling the
/// user about anything they need to act on.
#[derive(Default, Debug)]
pub struct ImportReport {
    pub messages: Vec<(log::Level, String)>,
    /// Display names of AI accounts whose credentials could not be carried
    /// over and need a fresh sign-in.
    pub accounts_needing_sign_in: Vec<String>,
}

impl ImportReport {
    fn info(&mut self, message: impl Into<String>) {
        self.messages.push((log::Level::Info, message.into()));
    }

    fn warn(&mut self, message: impl Into<String>) {
        self.messages.push((log::Level::Warn, message.into()));
    }
}

static REPORT: Mutex<Option<ImportReport>> = Mutex::new(None);

/// Returns the report from [`import_from_zed`], once. The import runs before
/// logging is initialized, so the caller logs these messages.
pub fn take_report() -> Option<ImportReport> {
    REPORT.lock().take()
}

/// Zed's data directory, if Lathe uses a different one. Lathe-managed AI
/// accounts created before the import still live under it.
pub fn legacy_data_dir() -> Option<PathBuf> {
    legacy_sibling(paths::data_dir())
}

/// Runs the import. Call once at startup, after any `--user-data-dir` override
/// is applied and before anything creates Lathe's directories.
pub fn import_from_zed(release_channel: &str, has_custom_data_dir: bool) {
    if has_custom_data_dir || release_channel == "dev" || *zed_env_vars::ZED_STATELESS {
        return;
    }
    let Some(roots) = Roots::current() else {
        return;
    };
    let mut report = ImportReport::default();
    if cfg!(any(target_os = "linux", target_os = "freebsd"))
        && let Err(error) = relocate_linux_data(&roots, &mut report)
    {
        report.warn(format!(
            "moving Lathe's data out of its install folder failed: {error:#}"
        ));
    }
    if let Err(error) = import(&roots, release_channel, &mut report) {
        report.warn(format!("importing data from Zed failed: {error:#}"));
    }
    *REPORT.lock() = Some(report);
}

#[derive(Clone, Debug)]
struct Roots {
    legacy_config: PathBuf,
    legacy_data: PathBuf,
    config: PathBuf,
    data: PathBuf,
}

impl Roots {
    fn current() -> Option<Self> {
        let config = paths::config_dir().clone();
        let data = paths::data_dir().clone();
        Some(Self {
            legacy_config: legacy_sibling(&config)?,
            legacy_data: legacy_sibling(&data)?,
            config,
            data,
        })
    }
}

/// Every platform puts the app's directories in a folder named after
/// `APP_NAME` (or its lowercase form), so Zed's folder is the sibling with
/// Zed's name. Deriving it this way keeps the platform logic in one place.
fn legacy_sibling(directory: &Path) -> Option<PathBuf> {
    let name = directory.file_name()?.to_str()?;
    let legacy_name = if name == paths::APP_NAME {
        LEGACY_APP_NAME
    } else if name == paths::APP_NAME_LOWERCASE || name == paths::LINUX_DATA_DIR_NAME {
        LEGACY_APP_NAME_LOWERCASE
    } else {
        return None;
    };
    if legacy_name == name {
        return None;
    }
    Some(directory.with_file_name(legacy_name))
}

/// What script/package-fork-linux puts in an installation. Everything else in
/// the old data folder is data.
const LINUX_INSTALL_ENTRIES: &[&str] = &["bin", "libexec", "lib", "share", "licenses.md"];

/// Lathe 1.11.0 and 1.11.1 kept Linux data in `~/.local/share/lathe`, the
/// folder script/install-fork-linux and auto-update install Lathe Stable into
/// and clear on every update. This moves that data to
/// [`paths::LINUX_DATA_DIR_NAME`], leaving the installation's files behind.
/// Entries are renamed one at a time, and the import marker last, so an
/// interrupted move resumes on the next launch.
fn relocate_linux_data(roots: &Roots, report: &mut ImportReport) -> Result<()> {
    if roots.data.file_name().and_then(|name| name.to_str()) != Some(paths::LINUX_DATA_DIR_NAME) {
        return Ok(());
    }
    let old = roots.data.with_file_name(paths::APP_NAME_LOWERCASE);
    let holds_data = old.join(MARKER_FILE_NAME).exists() || old.join("db").is_dir();
    if !holds_data || roots.data.join(MARKER_FILE_NAME).exists() {
        return Ok(());
    }
    fs::create_dir_all(&roots.data)
        .with_context(|| format!("creating {}", roots.data.display()))?;

    let mut entries = Vec::new();
    for entry in fs::read_dir(&old).with_context(|| format!("reading {}", old.display()))? {
        let name = entry?.file_name();
        let is_install_entry = name
            .to_str()
            .is_some_and(|name| LINUX_INSTALL_ENTRIES.contains(&name));
        if !is_install_entry && name != MARKER_FILE_NAME && name != LOCK_FILE_NAME {
            entries.push(name);
        }
    }
    if old.join(MARKER_FILE_NAME).exists() {
        entries.push(MARKER_FILE_NAME.into());
    }

    for name in entries {
        let source = old.join(&name);
        let destination = roots.data.join(&name);
        if destination.exists() {
            report.warn(format!(
                "not moving {} because {} already exists",
                source.display(),
                destination.display()
            ));
            continue;
        }
        match fs::rename(&source, &destination) {
            Ok(()) => {}
            // Another instance moved it first.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(anyhow!(error).context(format!(
                    "moving {} to {}",
                    source.display(),
                    destination.display()
                )));
            }
        }
    }

    rewrite_ai_account_dirs(&roots.config.join("ai_accounts.json"), &old, &roots.data)?;
    report.info(format!(
        "moved Lathe's data from {} to {}",
        old.display(),
        roots.data.display()
    ));
    Ok(())
}

/// Accounts record their config folder as an absolute path, so the ones Lathe
/// created under the old data folder have to follow it.
fn rewrite_ai_account_dirs(index_path: &Path, old: &Path, new: &Path) -> Result<()> {
    let contents = match fs::read(index_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut index: serde_json::Value =
        serde_json::from_slice(&contents).context("parsing ai_accounts.json")?;
    let mut changed = false;
    for account in index
        .get_mut("accounts")
        .and_then(|accounts| accounts.as_array_mut())
        .into_iter()
        .flatten()
    {
        let Some(config_dir) = account.get_mut("config_dir") else {
            continue;
        };
        let Some(relative) = config_dir
            .as_str()
            .and_then(|path| Path::new(path).strip_prefix(old).ok())
        else {
            continue;
        };
        *config_dir = serde_json::Value::String(new.join(relative).to_string_lossy().into_owned());
        changed = true;
    }
    if changed {
        write_atomically(index_path, &serde_json::to_vec_pretty(&index)?)?;
    }
    Ok(())
}

#[derive(Default, Serialize, Deserialize)]
struct Marker {
    /// Lathe's folders already held data when the import first ran, so nothing
    /// was copied over them.
    adopted_existing: bool,
    completed: BTreeSet<String>,
    attempts: BTreeMap<String, u32>,
}

impl Marker {
    fn load(path: &Path) -> Result<Option<Self>> {
        match fs::read(path) {
            Ok(contents) => Ok(Some(serde_json::from_slice(&contents)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn save(&self, path: &Path) -> Result<()> {
        write_atomically(path, &serde_json::to_vec_pretty(self)?)
    }

    fn should_attempt(&self, key: &str) -> bool {
        !self.completed.contains(key) && self.attempts.get(key).copied().unwrap_or(0) < MAX_ATTEMPTS
    }

    fn record(&mut self, key: &str, result: Result<()>, report: &mut ImportReport) {
        match result {
            Ok(()) => {
                self.completed.insert(key.to_string());
                self.attempts.remove(key);
            }
            Err(error) => {
                let attempts = self.attempts.entry(key.to_string()).or_default();
                *attempts += 1;
                report.warn(format!(
                    "importing {key} from Zed failed (attempt {attempts} of {MAX_ATTEMPTS}): {error:#}"
                ));
            }
        }
    }
}

fn import(roots: &Roots, release_channel: &str, report: &mut ImportReport) -> Result<()> {
    if !roots.legacy_config.exists() && !roots.legacy_data.exists() {
        return Ok(());
    }

    fs::create_dir_all(&roots.data)
        .with_context(|| format!("creating {}", roots.data.display()))?;
    let Some(_lock) = ImportLock::acquire(&roots.data.join(LOCK_FILE_NAME))? else {
        report.info("another Lathe instance is importing data from Zed; skipping");
        return Ok(());
    };

    let marker_path = roots.data.join(MARKER_FILE_NAME);
    let loaded_marker = Marker::load(&marker_path)?;
    let imported_on_earlier_launch = loaded_marker.is_some();
    let mut marker = match loaded_marker {
        Some(marker) => marker,
        None => {
            let marker = Marker {
                adopted_existing: roots.config.join("settings.json").exists()
                    || roots.data.join("db").exists(),
                ..Marker::default()
            };
            // Saved before copying anything, so a crash halfway through is
            // retried next launch instead of being mistaken for existing data.
            marker.save(&marker_path)?;
            if !marker.adopted_existing {
                report.info(format!(
                    "importing data from {} and {}",
                    roots.legacy_config.display(),
                    roots.legacy_data.display()
                ));
            }
            marker
        }
    };

    // Scopes no Lathe channel owns are copied by whichever channel imports
    // first, so that repairing Zed's copy never strips rows Lathe still needs.
    let database_scopes: Vec<String> = legacy_database_scopes(&roots.legacy_data)?
        .into_iter()
        .filter(|scope| !owned_by_other_channel(scope, release_channel))
        .collect();
    if !marker.adopted_existing {
        if marker.should_attempt("config") {
            let result = copy_config(roots, report);
            marker.record("config", result, report);
            marker.save(&marker_path)?;
        }

        for entry in DATA_ENTRIES {
            if marker.should_attempt(entry) {
                let result = copy_entry(
                    &roots.legacy_data.join(entry),
                    &roots.data.join(entry),
                    CopyMode::Always,
                )
                .map(|_| ());
                marker.record(entry, result, report);
                marker.save(&marker_path)?;
            }
        }

        for entry in CLONE_ONLY_ENTRIES {
            if marker.should_attempt(entry) {
                let result = copy_entry(
                    &roots.legacy_data.join(entry),
                    &roots.data.join(entry),
                    CopyMode::OnlyIfFree,
                )
                .map(|copied| {
                    if copied == Copied::SkippedNotFree {
                        report.info(format!(
                            "{entry} not copied from Zed because this filesystem can't clone it; \
                             it will be downloaded when needed"
                        ));
                    }
                });
                marker.record(entry, result, report);
                marker.save(&marker_path)?;
            }
        }

        let threads = "threads/threads.db";
        if marker.should_attempt(threads) {
            let result = copy_database(
                &roots.legacy_data.join(threads),
                &roots.data.join(threads),
                marker.attempts.contains_key(threads),
            );
            marker.record(threads, result, report);
            marker.save(&marker_path)?;
        }

        for scope in &database_scopes {
            let key = database_key(scope);
            if marker.should_attempt(&key) {
                let relative = database_relative_path(scope);
                let destination = roots.data.join(&relative);
                // Lathe Beta 1.11.0's copy predates everything Lathe Stable
                // wrote since, and nothing has opened it yet.
                let result = if scope == "stable" && marker.completed.contains("db/0-stable") {
                    ["", "-wal", "-shm", "-journal"]
                        .into_iter()
                        .try_for_each(|suffix| {
                            remove_if_exists(&path_with_suffix(&destination, suffix))
                        })
                } else {
                    Ok(())
                };
                let after_failed_attempt = marker.attempts.contains_key(&key);
                let result = result.and_then(|()| {
                    copy_database(
                        &roots.legacy_data.join(&relative),
                        &destination,
                        after_failed_attempt,
                    )
                });
                marker.record(&key, result, report);
                marker.save(&marker_path)?;
            }
        }

        #[cfg(target_os = "windows")]
        if marker.should_attempt("windows_credentials") {
            let result = copy_windows_credentials(report);
            marker.record("windows_credentials", result, report);
            marker.save(&marker_path)?;
        }
    }

    // Lathe Beta imports the items no channel owns (config, extensions,
    // threads, the global database) while Lathe Stable may still be on Zed's
    // folders and writing to them. Stable's first import catches Lathe's copy
    // up with anything it changed since. A first import by Stable itself
    // already has the latest, so this is recorded as done.
    if release_channel == "stable" && marker.should_attempt(STABLE_REFRESH_KEY) {
        let result = if imported_on_earlier_launch {
            refresh_shared_items_from_zed(roots, &database_scopes, report)
        } else {
            Ok(())
        };
        marker.record(STABLE_REFRESH_KEY, result, report);
        marker.save(&marker_path)?;
    }

    for scope in ZED_DATABASE_SCOPES {
        if owned_by_other_channel(scope, release_channel) {
            continue;
        }
        // Repairing removes Lathe's rows from Zed's copy, so only do it once
        // Lathe has its own copy of that scope.
        let lathe_copy_pending = database_scopes.iter().any(|copied| copied == scope)
            && !marker.adopted_existing
            && !marker.completed.contains(&database_key(scope));
        let key = repair_key(scope);
        if lathe_copy_pending || !marker.should_attempt(&key) {
            continue;
        }
        let result = repair_zed_database(&roots.legacy_data.join(database_relative_path(scope)))
            .map(|repaired| {
                if repaired {
                    report.info(format!(
                        "repaired stock Zed's {scope} database so Zed can open it again"
                    ));
                }
            });
        marker.record(&key, result, report);
        marker.save(&marker_path)?;
    }

    Ok(())
}

/// Copies every config and data file Zed changed after Lathe's copy was made,
/// setting Lathe's version aside first. Databases are compared by their newest
/// file, since recent writes may only be in the `-wal`.
fn refresh_shared_items_from_zed(
    roots: &Roots,
    database_scopes: &[String],
    report: &mut ImportReport,
) -> Result<()> {
    let mut refreshed = Vec::new();
    if roots.legacy_config.is_dir() {
        refresh_directory(&roots.legacy_config, &roots.config, &mut refreshed)?;
        expire_unreachable_ai_accounts(&roots.config.join("ai_accounts.json"), report)?;
    }
    for entry in DATA_ENTRIES {
        let source = roots.legacy_data.join(entry);
        let destination = roots.data.join(entry);
        if source.is_dir() {
            refresh_directory(&source, &destination, &mut refreshed)?;
        } else if source.is_file() {
            refresh_file(&source, &destination, &mut refreshed)?;
        }
    }

    let shared_databases = std::iter::once(PathBuf::from("threads/threads.db")).chain(
        database_scopes
            .iter()
            .filter(|scope| !LATHE_CHANNEL_SCOPES.contains(&scope.as_str()))
            .map(|scope| database_relative_path(scope)),
    );
    for relative in shared_databases {
        let source = roots.legacy_data.join(&relative);
        let destination = roots.data.join(&relative);
        if source.exists() && newest_database_write(&source)? > newest_database_write(&destination)?
        {
            copy_database(&source, &destination, true)?;
            refreshed.push(destination);
        }
    }

    #[cfg(target_os = "windows")]
    copy_windows_credentials(report)?;

    for path in refreshed {
        report.info(format!(
            "updated {} with Lathe Stable's newer copy",
            path.display()
        ));
    }
    Ok(())
}

fn refresh_directory(
    source: &Path,
    destination: &Path,
    refreshed: &mut Vec<PathBuf>,
) -> Result<()> {
    for entry in fs::read_dir(source).with_context(|| format!("reading {}", source.display()))? {
        let entry = entry?;
        let source_path = entry.path();
        if is_temporary(&source_path) {
            continue;
        }
        let destination_path = destination.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            refresh_directory(&source_path, &destination_path, refreshed)?;
        } else if file_type.is_file() {
            refresh_file(&source_path, &destination_path, refreshed)?;
        }
    }
    Ok(())
}

fn refresh_file(source: &Path, destination: &Path, refreshed: &mut Vec<PathBuf>) -> Result<()> {
    if let Ok(metadata) = destination.symlink_metadata() {
        if !metadata.is_file()
            || fs::metadata(source)?.modified()? <= metadata.modified()?
            || fs::read(source)? == fs::read(destination)?
        {
            return Ok(());
        }
        let set_aside = path_with_suffix(destination, &format!(".{STABLE_REFRESH_BACKUP_SUFFIX}"));
        remove_if_exists(&set_aside)?;
        fs::rename(destination, &set_aside)
            .with_context(|| format!("setting aside {}", destination.display()))?;
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = temporary_path(destination);
    fs::copy(source, &temporary).with_context(|| format!("copying {}", source.display()))?;
    fs::rename(&temporary, destination)?;
    refreshed.push(destination.to_path_buf());
    Ok(())
}

fn newest_database_write(path: &Path) -> Result<Option<SystemTime>> {
    let mut newest = None;
    for suffix in ["", "-wal"] {
        match fs::metadata(path_with_suffix(path, suffix)) {
            Ok(metadata) => newest = newest.max(Some(metadata.modified()?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(newest)
}

/// Scopes under Zed's `db/`, from directories named `0-<scope>`. The Dev
/// channel's scope is skipped because Dev builds never import.
fn legacy_database_scopes(legacy_data: &Path) -> Result<Vec<String>> {
    let entries = match fs::read_dir(legacy_data.join("db")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut scopes = Vec::new();
    for entry in entries {
        let name = entry?.file_name();
        if let Some(scope) = name.to_str().and_then(|name| name.strip_prefix("0-"))
            && scope != "dev"
        {
            scopes.push(scope.to_string());
        }
    }
    scopes.sort();
    Ok(scopes)
}

fn owned_by_other_channel(scope: &str, release_channel: &str) -> bool {
    scope != release_channel && LATHE_CHANNEL_SCOPES.contains(&scope)
}

// Lathe Beta 1.11.0 copied and repaired the stable scope while Lathe Stable
// still used Zed's folders, and recorded both under the plain keys. The stable
// scope's keys differ so Lathe Stable's own import redoes that work.
fn database_key(scope: &str) -> String {
    if scope == "stable" {
        format!("db/0-{scope}/stable")
    } else {
        format!("db/0-{scope}")
    }
}

fn repair_key(scope: &str) -> String {
    if scope == "stable" {
        format!("repair_zed_db/0-{scope}/stable")
    } else {
        format!("repair_zed_db/0-{scope}")
    }
}

fn database_relative_path(scope: &str) -> PathBuf {
    Path::new("db").join(format!("0-{scope}")).join("db.sqlite")
}

fn copy_config(roots: &Roots, report: &mut ImportReport) -> Result<()> {
    if !roots.legacy_config.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(&roots.config)
        .with_context(|| format!("creating {}", roots.config.display()))?;
    merge_directory(&roots.legacy_config, &roots.config)?;
    expire_unreachable_ai_accounts(&roots.config.join("ai_accounts.json"), report)
}

/// Copies files from `source` that don't exist yet in `destination`, one file
/// at a time, so a crash never leaves a half-written settings file.
fn merge_directory(source: &Path, destination: &Path) -> Result<()> {
    for entry in fs::read_dir(source).with_context(|| format!("reading {}", source.display()))? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if is_temporary(&source_path) {
            continue;
        }
        let file_type = entry.file_type()?;
        // An existing real directory is merged into rather than skipped, so a
        // folder a failed attempt only partly copied gets finished.
        let destination_is_directory = destination_path
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.is_dir());
        if file_type.is_dir() && destination_is_directory {
            merge_directory(&source_path, &destination_path)?;
            continue;
        }
        if destination_path.symlink_metadata().is_ok() {
            continue;
        }
        if file_type.is_symlink() {
            copy_symlink(&source_path, &destination_path)?;
        } else if file_type.is_dir() {
            fs::create_dir_all(&destination_path)?;
            merge_directory(&source_path, &destination_path)?;
        } else {
            let temporary = temporary_path(&destination_path);
            fs::copy(&source_path, &temporary)
                .with_context(|| format!("copying {}", source_path.display()))?;
            fs::rename(&temporary, &destination_path)?;
        }
    }
    Ok(())
}

/// Accounts are carried over by reference: `ai_accounts.json` keeps pointing at
/// each account's existing directory. If that directory is gone, its sign-in is
/// too, so flag just that account instead of leaving it looking signed in.
fn expire_unreachable_ai_accounts(index_path: &Path, report: &mut ImportReport) -> Result<()> {
    let contents = match fs::read(index_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut index: serde_json::Value = serde_json::from_slice(&contents)?;
    let Some(accounts) = index
        .get_mut("accounts")
        .and_then(|accounts| accounts.as_array_mut())
    else {
        return Ok(());
    };

    let mut changed = false;
    for account in accounts {
        let is_authenticated =
            account.get("state").and_then(|state| state.as_str()) == Some("authenticated");
        let directory_exists = account
            .get("config_dir")
            .and_then(|directory| directory.as_str())
            .is_some_and(|directory| Path::new(directory).is_dir());
        if is_authenticated && !directory_exists {
            if let Some(name) = account.get("display_name").and_then(|name| name.as_str()) {
                report.accounts_needing_sign_in.push(name.to_string());
            }
            account["state"] = serde_json::Value::from("expired");
            changed = true;
        }
    }

    if changed {
        write_atomically(index_path, &serde_json::to_vec_pretty(&index)?)?;
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CopyMode {
    Always,
    OnlyIfFree,
}

#[derive(Debug, PartialEq, Eq)]
enum Copied {
    Yes,
    NothingToCopy,
    SkippedNotFree,
}

/// Copies a file or directory into place via a temporary name, so the
/// destination either has the whole entry or nothing.
fn copy_entry(source: &Path, destination: &Path, mode: CopyMode) -> Result<Copied> {
    let Ok(metadata) = source.symlink_metadata() else {
        return Ok(Copied::NothingToCopy);
    };
    if destination.symlink_metadata().is_ok() {
        return Ok(Copied::NothingToCopy);
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = temporary_path(destination);
    remove_if_exists(&temporary)?;

    let copied = if clone_tree(source, &temporary)? {
        Copied::Yes
    } else if mode == CopyMode::OnlyIfFree {
        remove_if_exists(&temporary)?;
        return Ok(Copied::SkippedNotFree);
    } else {
        if metadata.is_dir() {
            copy_tree(source, &temporary)?;
        } else {
            fs::copy(source, &temporary)
                .with_context(|| format!("copying {}", source.display()))?;
        }
        Copied::Yes
    };
    fs::rename(&temporary, destination)
        .with_context(|| format!("moving {} into place", destination.display()))?;
    Ok(copied)
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source).with_context(|| format!("reading {}", source.display()))? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            copy_symlink(&source_path, &destination_path)?;
        } else if file_type.is_dir() {
            copy_tree(&source_path, &destination_path)?;
        } else {
            fs::copy(&source_path, &destination_path)
                .with_context(|| format!("copying {}", source_path.display()))?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn copy_symlink(source: &Path, destination: &Path) -> Result<()> {
    std::os::unix::fs::symlink(fs::read_link(source)?, destination)?;
    Ok(())
}

#[cfg(not(unix))]
fn copy_symlink(source: &Path, destination: &Path) -> Result<()> {
    if source.is_dir() {
        copy_tree(source, destination)
    } else {
        fs::copy(source, destination)?;
        Ok(())
    }
}

/// Copy-on-write clone of a whole tree. Returns false if the filesystem can't
/// clone, leaving nothing at `destination`.
#[cfg(target_os = "macos")]
fn clone_tree(source: &Path, destination: &Path) -> Result<bool> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt as _};

    // From <sys/clonefile.h>; not exported by the libc crate.
    const CLONE_NOFOLLOW: u32 = 0x0001;

    let source_path = CString::new(source.as_os_str().as_bytes())?;
    let destination_path = CString::new(destination.as_os_str().as_bytes())?;
    // SAFETY: both arguments are valid NUL-terminated paths. clonefile copies a
    // directory hierarchy as if each item were cloned individually.
    let result = unsafe {
        libc::clonefile(
            source_path.as_ptr(),
            destination_path.as_ptr(),
            CLONE_NOFOLLOW,
        )
    };
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ENOTSUP) | Some(libc::EXDEV) => {
            remove_if_exists(destination)?;
            Ok(false)
        }
        _ => Err(anyhow!(error).context(format!("cloning {}", source.display()))),
    }
}

#[cfg(target_os = "linux")]
fn clone_tree(source: &Path, destination: &Path) -> Result<bool> {
    fn clone_recursively(source: &Path, destination: &Path) -> Result<bool> {
        use std::os::unix::io::AsRawFd as _;

        let metadata = source.symlink_metadata()?;
        if metadata.is_symlink() {
            copy_symlink(source, destination)?;
            return Ok(true);
        }
        if metadata.is_dir() {
            fs::create_dir_all(destination)?;
            fs::set_permissions(destination, metadata.permissions())?;
            for entry in fs::read_dir(source)? {
                let entry = entry?;
                if !clone_recursively(&entry.path(), &destination.join(entry.file_name()))? {
                    return Ok(false);
                }
            }
            return Ok(true);
        }
        let source_file = fs::File::open(source)?;
        let destination_file = fs::File::create(destination)?;
        // SAFETY: both descriptors are open for the duration of the call.
        let result = unsafe {
            libc::ioctl(
                destination_file.as_raw_fd(),
                libc::FICLONE,
                source_file.as_raw_fd(),
            )
        };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            return match error.raw_os_error() {
                Some(libc::EOPNOTSUPP)
                | Some(libc::EXDEV)
                | Some(libc::EINVAL)
                | Some(libc::ENOTTY) => Ok(false),
                _ => Err(anyhow!(error).context(format!("cloning {}", source.display()))),
            };
        }
        fs::set_permissions(destination, metadata.permissions())?;
        Ok(true)
    }

    if clone_recursively(source, destination)? {
        Ok(true)
    } else {
        remove_if_exists(destination)?;
        Ok(false)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn clone_tree(_source: &Path, _destination: &Path) -> Result<bool> {
    Ok(false)
}

/// Copies a SQLite database with the online backup API. A plain file copy can
/// miss committed data still in the `-wal` file, or catch a write halfway.
/// Copies one database. `after_failed_attempt` means an earlier attempt
/// failed, so any file at `destination` is one Lathe created empty when it
/// started without the import. It is set aside, not kept, otherwise the retry
/// would see it and record the import as done.
fn copy_database(source: &Path, destination: &Path, after_failed_attempt: bool) -> Result<()> {
    if !source.exists() || (destination.exists() && !after_failed_attempt) {
        return Ok(());
    }
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", destination.display()))?;
    fs::create_dir_all(parent)?;
    if destination.exists() {
        for suffix in ["", "-wal", "-shm"] {
            let path = path_with_suffix(destination, suffix);
            if path.exists() {
                let set_aside = path_with_suffix(
                    destination,
                    &format!(".{FAILED_IMPORT_BACKUP_SUFFIX}{suffix}"),
                );
                remove_if_exists(&set_aside)?;
                fs::rename(&path, &set_aside)
                    .with_context(|| format!("setting aside {}", path.display()))?;
            }
        }
    }
    // Leftovers from a crashed attempt would be applied to the new file.
    for suffix in ["-wal", "-shm", "-journal"] {
        remove_if_exists(&path_with_suffix(destination, suffix))?;
    }
    let temporary = temporary_path(destination);
    remove_if_exists(&temporary)?;

    {
        let source_database = sqlite::Database::open(source, sqlite::OpenMode::ReadOnly)?;
        let destination_database =
            sqlite::Database::open(&temporary, sqlite::OpenMode::ReadWriteCreate)?;
        source_database.backup_to(&destination_database)?;
    }
    fs::rename(&temporary, destination)
        .with_context(|| format!("moving {} into place", destination.display()))?;
    Ok(())
}

/// Removes Lathe's migration rows from one of stock Zed's databases, so Zed
/// stops failing with "Migration changed" and falling back to an in-memory
/// database. Only the `migrations` table changes: Lathe's extra columns are
/// harmless to Zed, and upstream's steps are already applied. A backup is
/// written next to the file first. Returns whether anything was changed.
fn repair_zed_database(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let database = sqlite::Database::open(path, sqlite::OpenMode::ReadWrite)?;
    if !database.has_table("migrations")? {
        return Ok(false);
    }
    let lathe_rows = database.lathe_only_migration_rows()?;
    if lathe_rows.is_empty() {
        return Ok(false);
    }

    let backup_path = path_with_suffix(path, &format!(".{ZED_DATABASE_BACKUP_SUFFIX}"));
    if !backup_path.exists() {
        let temporary = temporary_path(&backup_path);
        remove_if_exists(&temporary)?;
        {
            let backup = sqlite::Database::open(&temporary, sqlite::OpenMode::ReadWriteCreate)?;
            database.backup_to(&backup)?;
        }
        fs::rename(&temporary, &backup_path)?;
    }

    database.execute("BEGIN IMMEDIATE")?;
    let result = (|| {
        // Highest step first, so the later rows' numbers are still right when
        // each one is removed.
        for (domain, step) in lathe_rows.iter().rev() {
            database.execute(&format!(
                "DELETE FROM migrations WHERE domain = '{domain}' AND step = {step}"
            ))?;
            for later_step in database.steps_after(domain, *step)? {
                database.execute(&format!(
                    "UPDATE migrations SET step = {} WHERE domain = '{domain}' AND step = {later_step}",
                    later_step - 1
                ))?;
            }
        }
        Ok(())
    })();
    match result {
        Ok(()) => database.execute("COMMIT")?,
        Err(error) => {
            database.execute("ROLLBACK").ok();
            return Err(error);
        }
    }
    Ok(true)
}

#[cfg(target_os = "windows")]
fn copy_windows_credentials(report: &mut ImportReport) -> Result<()> {
    use windows::{
        Win32::Security::Credentials::{
            CREDENTIALW, CredEnumerateW, CredFree, CredReadW, CredWriteW,
        },
        core::{PCWSTR, PWSTR},
    };

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    let filter = wide(&format!("{LEGACY_WINDOWS_CREDENTIAL_PREFIX}*"));
    let mut count = 0u32;
    let mut credentials: *mut *mut CREDENTIALW = std::ptr::null_mut();
    // SAFETY: the out-pointers are valid; on success `credentials` holds
    // `count` entries until `CredFree`.
    if let Err(error) =
        unsafe { CredEnumerateW(PCWSTR(filter.as_ptr()), None, &mut count, &mut credentials) }
    {
        // ERROR_NOT_FOUND: nothing stored under Zed's prefix.
        if error.code() == windows::Win32::Foundation::ERROR_NOT_FOUND.to_hresult() {
            return Ok(());
        }
        return Err(anyhow!("enumerating Windows credentials: {error}"));
    }

    let mut result = Ok(());
    // SAFETY: CredEnumerateW succeeded, so this is an array of `count` valid
    // pointers until `CredFree` below.
    let entries = unsafe { std::slice::from_raw_parts(credentials, count as usize) };
    for &entry in entries {
        // SAFETY: each entry is a valid CREDENTIALW.
        let credential = unsafe { &*entry };
        // SAFETY: TargetName is a valid NUL-terminated string.
        let Ok(target) = (unsafe { credential.TargetName.to_string() }) else {
            continue;
        };
        let Some(url) = target.strip_prefix(LEGACY_WINDOWS_CREDENTIAL_PREFIX) else {
            continue;
        };
        let mut new_target = wide(&format!("{WINDOWS_CREDENTIAL_PREFIX}{url}"));

        let mut existing: *mut CREDENTIALW = std::ptr::null_mut();
        // SAFETY: valid target and out-pointer.
        if unsafe {
            CredReadW(
                PCWSTR(new_target.as_ptr()),
                credential.Type,
                None,
                &mut existing,
            )
        }
        .is_ok()
        {
            // SAFETY: CredReadW succeeded.
            unsafe { CredFree(existing as *const _) };
            continue;
        }

        let copy = CREDENTIALW {
            TargetName: PWSTR::from_raw(new_target.as_mut_ptr()),
            TargetAlias: PWSTR::null(),
            ..*credential
        };
        // SAFETY: `copy` borrows the enumerated credential's buffers, which
        // stay valid until `CredFree`.
        if let Err(error) = unsafe { CredWriteW(&copy, 0) } {
            report.warn(format!("copying the Windows credential for {url}: {error}"));
            result = Err(anyhow!("some Windows credentials could not be copied"));
        }
    }
    // SAFETY: frees the array returned by CredEnumerateW.
    unsafe { CredFree(credentials as *const _) };
    result
}

/// Held while importing so two Lathe channels launching together don't both
/// import into the same folders.
struct ImportLock {
    path: PathBuf,
}

impl ImportLock {
    fn acquire(path: &Path) -> Result<Option<Self>> {
        let deadline = SystemTime::now() + LOCK_WAIT;
        loop {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(_) => {
                    return Ok(Some(Self {
                        path: path.to_path_buf(),
                    }));
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let is_stale = fs::metadata(path)
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age > STALE_LOCK_AGE);
                    if is_stale {
                        remove_if_exists(path)?;
                        continue;
                    }
                    if SystemTime::now() > deadline {
                        return Ok(None);
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(error) => {
                    return Err(anyhow!(error).context(format!("creating {}", path.display())));
                }
            }
        }
    }
}

impl Drop for ImportLock {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.path) {
            log::warn!("removing {}: {error}", self.path.display());
        }
    }
}

fn temporary_path(path: &Path) -> PathBuf {
    path_with_suffix(path, &format!(".{TEMPORARY_SUFFIX}"))
}

fn is_temporary(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == TEMPORARY_SUFFIX)
}

fn path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn remove_if_exists(path: &Path) -> Result<()> {
    let Ok(metadata) = path.symlink_metadata() else {
        return Ok(());
    };
    if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
    .with_context(|| format!("removing {}", path.display()))
}

fn write_atomically(path: &Path, contents: &[u8]) -> Result<()> {
    let temporary = temporary_path(path);
    fs::write(&temporary, contents).with_context(|| format!("writing {}", temporary.display()))?;
    fs::rename(&temporary, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// The few SQLite calls the import needs. `sqlez` isn't used because its
/// `open_file` silently falls back to an in-memory database and its backup
/// ignores busy errors, both of which would turn a failed import into a quiet
/// empty one.
mod sqlite {
    use super::LATHE_ONLY_MIGRATIONS;
    use anyhow::{Result, anyhow, bail};
    use libsqlite3_sys::*;
    use std::{
        ffi::{CStr, CString},
        path::Path,
        ptr,
        time::Duration,
    };

    const BUSY_TIMEOUT_MILLISECONDS: i32 = 5000;
    const BACKUP_BUSY_RETRIES: u32 = 20;

    pub enum OpenMode {
        ReadOnly,
        ReadWrite,
        ReadWriteCreate,
    }

    pub struct Database {
        handle: *mut sqlite3,
    }

    impl Database {
        pub fn open(path: &Path, mode: OpenMode) -> Result<Self> {
            let flags = match mode {
                OpenMode::ReadOnly => SQLITE_OPEN_READONLY,
                OpenMode::ReadWrite => SQLITE_OPEN_READWRITE,
                OpenMode::ReadWriteCreate => SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE,
            } | SQLITE_OPEN_NOMUTEX;
            let path_string = CString::new(path.to_string_lossy().as_bytes())?;
            let mut handle = ptr::null_mut();
            // SAFETY: valid path and out-pointer. The handle is closed in Drop
            // even when opening fails, as SQLite requires.
            let result =
                unsafe { sqlite3_open_v2(path_string.as_ptr(), &mut handle, flags, ptr::null()) };
            let database = Self { handle };
            if result != SQLITE_OK {
                bail!("opening {}: {}", path.display(), database.error_message());
            }
            // SAFETY: the handle is open.
            unsafe { sqlite3_busy_timeout(handle, BUSY_TIMEOUT_MILLISECONDS) };
            Ok(database)
        }

        fn error_message(&self) -> String {
            if self.handle.is_null() {
                return "out of memory".to_string();
            }
            // SAFETY: the handle is valid, and the message is valid until the
            // next call on it.
            unsafe { CStr::from_ptr(sqlite3_errmsg(self.handle)) }
                .to_string_lossy()
                .into_owned()
        }

        pub fn execute(&self, sql: &str) -> Result<()> {
            let sql_string = CString::new(sql)?;
            // SAFETY: valid handle and SQL; no callback.
            let result = unsafe {
                sqlite3_exec(
                    self.handle,
                    sql_string.as_ptr(),
                    None,
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            };
            if result != SQLITE_OK {
                bail!("{sql}: {}", self.error_message());
            }
            Ok(())
        }

        pub fn backup_to(&self, destination: &Database) -> Result<()> {
            let main = CString::new("main")?;
            // SAFETY: both handles are open; the backup is finished below.
            let backup = unsafe {
                sqlite3_backup_init(
                    destination.handle,
                    main.as_ptr(),
                    self.handle,
                    main.as_ptr(),
                )
            };
            if backup.is_null() {
                bail!("starting backup: {}", destination.error_message());
            }
            let mut retries = 0;
            let step_result = loop {
                // SAFETY: `backup` is valid until finished.
                let result = unsafe { sqlite3_backup_step(backup, -1) };
                if (result == SQLITE_BUSY || result == SQLITE_LOCKED)
                    && retries < BACKUP_BUSY_RETRIES
                {
                    retries += 1;
                    std::thread::sleep(Duration::from_millis(250));
                    continue;
                }
                break result;
            };
            // SAFETY: finishes the backup started above.
            let finish_result = unsafe { sqlite3_backup_finish(backup) };
            if step_result != SQLITE_DONE {
                bail!("backup failed: {}", self.error_message());
            }
            if finish_result != SQLITE_OK {
                bail!("backup failed: {}", destination.error_message());
            }
            Ok(())
        }

        pub fn query(&self, sql: &str, mut each_row: impl FnMut(&Row)) -> Result<()> {
            let sql_string = CString::new(sql)?;
            let mut statement = ptr::null_mut();
            // SAFETY: valid handle, SQL and out-pointer.
            let result = unsafe {
                sqlite3_prepare_v2(
                    self.handle,
                    sql_string.as_ptr(),
                    -1,
                    &mut statement,
                    ptr::null_mut(),
                )
            };
            if result != SQLITE_OK {
                bail!("{sql}: {}", self.error_message());
            }
            let outcome = loop {
                // SAFETY: the statement is valid until finalized.
                match unsafe { sqlite3_step(statement) } {
                    SQLITE_ROW => each_row(&Row { statement }),
                    SQLITE_DONE => break Ok(()),
                    _ => break Err(anyhow!("{sql}: {}", self.error_message())),
                }
            };
            // SAFETY: finalizes the statement prepared above.
            unsafe { sqlite3_finalize(statement) };
            outcome
        }

        pub fn has_table(&self, name: &str) -> Result<bool> {
            let mut found = false;
            self.query(
                &format!("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '{name}'"),
                |_| found = true,
            )?;
            Ok(found)
        }

        /// `(domain, step)` of every stored migration Lathe added, in
        /// ascending step order per domain.
        pub fn lathe_only_migration_rows(&self) -> Result<Vec<(&'static str, i64)>> {
            let mut rows = Vec::new();
            for (domain, fragment) in LATHE_ONLY_MIGRATIONS {
                self.query(
                    &format!(
                        "SELECT step, migration FROM migrations WHERE domain = '{domain}' ORDER BY step"
                    ),
                    |row| {
                        let normalized =
                            row.text(1).split_whitespace().collect::<Vec<_>>().join(" ");
                        if normalized.contains(fragment) {
                            rows.push((*domain, row.integer(0)));
                        }
                    },
                )?;
            }
            Ok(rows)
        }

        pub fn steps_after(&self, domain: &str, step: i64) -> Result<Vec<i64>> {
            let mut steps = Vec::new();
            self.query(
                &format!(
                    "SELECT step FROM migrations WHERE domain = '{domain}' AND step > {step} ORDER BY step"
                ),
                |row| steps.push(row.integer(0)),
            )?;
            Ok(steps)
        }
    }

    impl Drop for Database {
        fn drop(&mut self) {
            // SAFETY: closing a valid or null handle is allowed.
            unsafe { sqlite3_close(self.handle) };
        }
    }

    pub struct Row {
        statement: *mut sqlite3_stmt,
    }

    impl Row {
        pub fn integer(&self, column: i32) -> i64 {
            // SAFETY: the statement is on a row.
            unsafe { sqlite3_column_int64(self.statement, column) }
        }

        pub fn text(&self, column: i32) -> String {
            // SAFETY: the statement is on a row; a NULL column returns null.
            let text = unsafe { sqlite3_column_text(self.statement, column) };
            if text.is_null() {
                return String::new();
            }
            // SAFETY: SQLite returns a NUL-terminated string valid until the
            // next step.
            unsafe { CStr::from_ptr(text.cast()) }
                .to_string_lossy()
                .into_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    struct Fixture {
        _root: tempfile::TempDir,
        roots: Roots,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().expect("temp dir");
            let path = root.path();
            let roots = Roots {
                legacy_config: path.join("config/zed"),
                legacy_data: path.join("data/Zed"),
                config: path.join("config/lathe"),
                data: path.join("data/Lathe"),
            };
            Self { _root: root, roots }
        }

        fn write_legacy_config(&self, relative: &str, contents: &str) {
            write(&self.roots.legacy_config.join(relative), contents);
        }

        fn write_legacy_data(&self, relative: &str, contents: &str) {
            write(&self.roots.legacy_data.join(relative), contents);
        }

        fn import(&self) -> ImportReport {
            self.import_as("stable")
        }

        fn import_as(&self, release_channel: &str) -> ImportReport {
            let mut report = ImportReport::default();
            import(&self.roots, release_channel, &mut report).expect("import");
            report
        }

        fn marker(&self) -> Marker {
            Marker::load(&self.roots.data.join(MARKER_FILE_NAME))
                .expect("marker readable")
                .expect("marker exists")
        }
    }

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        fs::write(path, contents).expect("write");
    }

    /// Every file under `root` with its size, modification time and contents,
    /// excluding SQLite's own `-wal`/`-shm` files, which any reader may create.
    fn snapshot(root: &Path) -> BTreeMap<PathBuf, (u64, u128, Vec<u8>)> {
        fn walk(path: &Path, into: &mut BTreeMap<PathBuf, (u64, u128, Vec<u8>)>) {
            let Ok(entries) = fs::read_dir(path) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = path.to_string_lossy();
                if name.ends_with("-wal") || name.ends_with("-shm") {
                    continue;
                }
                let metadata = entry.metadata().expect("metadata");
                if metadata.is_dir() {
                    walk(&path, into);
                } else {
                    let modified = metadata
                        .modified()
                        .expect("mtime")
                        .duration_since(UNIX_EPOCH)
                        .expect("after epoch")
                        .as_nanos();
                    let contents = fs::read(&path).expect("read");
                    into.insert(path, (metadata.len(), modified, contents));
                }
            }
        }
        let mut files = BTreeMap::new();
        walk(root, &mut files);
        files
    }

    fn legacy_snapshot(fixture: &Fixture) -> Vec<BTreeMap<PathBuf, (u64, u128, Vec<u8>)>> {
        vec![
            snapshot(&fixture.roots.legacy_config),
            snapshot(&fixture.roots.legacy_data),
        ]
    }

    fn create_database(path: &Path, statements: &[&str]) {
        fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        let database =
            sqlite::Database::open(path, sqlite::OpenMode::ReadWriteCreate).expect("open database");
        for statement in statements {
            database.execute(statement).expect("execute");
        }
    }

    fn migration_rows(path: &Path, domain: &str) -> Vec<(i64, String)> {
        let database =
            sqlite::Database::open(path, sqlite::OpenMode::ReadOnly).expect("open database");
        let mut rows = Vec::new();
        database
            .query(
                &format!(
                    "SELECT step, migration FROM migrations WHERE domain = '{domain}' ORDER BY step"
                ),
                |row| rows.push((row.integer(0), row.text(1))),
            )
            .expect("query");
        rows
    }

    fn count_rows(path: &Path, table: &str) -> i64 {
        let database =
            sqlite::Database::open(path, sqlite::OpenMode::ReadOnly).expect("open database");
        let mut count = 0;
        database
            .query(&format!("SELECT COUNT(*) FROM {table}"), |row| {
                count = row.integer(0)
            })
            .expect("query");
        count
    }

    #[test]
    fn nothing_to_import_creates_nothing() {
        let fixture = Fixture::new();
        fixture.import();
        assert!(!fixture.roots.data.exists());
        assert!(!fixture.roots.config.exists());
    }

    #[test]
    fn copies_config_and_data_without_touching_zed() {
        let fixture = Fixture::new();
        fixture.write_legacy_config("settings.json", r#"{"theme": "Lathe"}"#);
        fixture.write_legacy_config("keymap.json", "[]");
        fixture.write_legacy_config("themes/mine.json", "{}");
        fixture.write_legacy_data("extensions/installed/html/extension.toml", "id = 'html'");
        fixture.write_legacy_data("extensions/index.json", "{}");
        fixture.write_legacy_data("extensions/staging/half/file", "x");
        fixture.write_legacy_data("ai_accounts/claude-acp/one/settings.json", "{}");
        fixture.write_legacy_data("node/stable/bin/node", "x");
        let before = legacy_snapshot(&fixture);

        fixture.import();

        assert_eq!(legacy_snapshot(&fixture), before);
        let read = |path: PathBuf| fs::read_to_string(path).expect("copied");
        assert_eq!(
            read(fixture.roots.config.join("settings.json")),
            r#"{"theme": "Lathe"}"#
        );
        assert_eq!(read(fixture.roots.config.join("themes/mine.json")), "{}");
        assert_eq!(
            read(
                fixture
                    .roots
                    .data
                    .join("extensions/installed/html/extension.toml")
            ),
            "id = 'html'"
        );
        assert!(fixture.roots.data.join("extensions/index.json").exists());
        assert!(!fixture.roots.data.join("extensions/staging").exists());
        assert!(!fixture.roots.data.join("ai_accounts").exists());
        assert!(!fixture.roots.data.join("node").exists());
        let marker = fixture.marker();
        assert!(!marker.adopted_existing);
        assert!(marker.completed.contains("config"));
        assert!(marker.attempts.is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn clones_caches_where_free() {
        let fixture = Fixture::new();
        fixture.write_legacy_data("languages/rust-analyzer/rust-analyzer", "binary");
        let before = legacy_snapshot(&fixture);

        fixture.import();

        assert_eq!(legacy_snapshot(&fixture), before);
        assert_eq!(
            fs::read_to_string(
                fixture
                    .roots
                    .data
                    .join("languages/rust-analyzer/rust-analyzer")
            )
            .expect("cloned"),
            "binary"
        );
    }

    #[test]
    fn existing_lathe_data_is_not_overwritten() {
        let fixture = Fixture::new();
        fixture.write_legacy_config("settings.json", "zed");
        write(&fixture.roots.config.join("settings.json"), "lathe");

        fixture.import();

        assert_eq!(
            fs::read_to_string(fixture.roots.config.join("settings.json")).expect("read"),
            "lathe"
        );
        assert!(fixture.marker().adopted_existing);
    }

    #[test]
    fn runs_once() {
        let fixture = Fixture::new();
        fixture.write_legacy_config("settings.json", "first");
        fixture.import();
        fs::remove_file(fixture.roots.config.join("settings.json")).expect("remove");

        fixture.import();

        assert!(!fixture.roots.config.join("settings.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn failed_items_are_retried_then_given_up() {
        use std::os::unix::fs::PermissionsExt as _;

        let fixture = Fixture::new();
        fixture.write_legacy_config("settings.json", "{}");
        fixture.write_legacy_data("prompts/prompt.md", "hello");
        let prompts = fixture.roots.legacy_data.join("prompts");
        fs::set_permissions(&prompts, fs::Permissions::from_mode(0o000)).expect("chmod");

        fixture.import();
        let marker = fixture.marker();
        assert!(marker.completed.contains("config"));
        assert_eq!(marker.attempts.get("prompts"), Some(&1));

        fs::set_permissions(&prompts, fs::Permissions::from_mode(0o755)).expect("chmod");
        fixture.import();
        let marker = fixture.marker();
        assert!(marker.completed.contains("prompts"));
        assert!(marker.attempts.is_empty());
        assert_eq!(
            fs::read_to_string(fixture.roots.data.join("prompts/prompt.md")).expect("read"),
            "hello"
        );
    }

    #[test]
    fn stable_catches_up_on_what_it_changed_after_betas_import() {
        let fixture = Fixture::new();
        fixture.write_legacy_config("settings.json", "{ \"theme\": \"old\" }");
        fixture.write_legacy_config("keymap.json", "[]");
        let threads = fixture.roots.legacy_data.join("threads/threads.db");
        create_database(&threads, &["CREATE TABLE threads (id INTEGER)"]);
        fixture.import_as("beta");

        // Lathe Stable, still on Zed's folders, keeps writing after that.
        let later = SystemTime::now() + Duration::from_secs(60);
        fixture.write_legacy_config("settings.json", "{ \"theme\": \"new\" }");
        create_database(&threads, &["INSERT INTO threads VALUES (1)"]);
        for path in [
            fixture.roots.legacy_config.join("settings.json"),
            threads.clone(),
        ] {
            fs::File::options()
                .write(true)
                .open(&path)
                .and_then(|file| file.set_modified(later))
                .expect("set mtime");
        }
        remove_if_exists(&path_with_suffix(&threads, "-wal")).expect("remove wal");

        fixture.import_as("stable");

        let settings = fixture.roots.config.join("settings.json");
        assert_eq!(
            fs::read_to_string(&settings).expect("read"),
            "{ \"theme\": \"new\" }"
        );
        assert_eq!(
            fs::read_to_string(path_with_suffix(
                &settings,
                &format!(".{STABLE_REFRESH_BACKUP_SUFFIX}")
            ))
            .expect("read backup"),
            "{ \"theme\": \"old\" }"
        );
        assert!(
            !path_with_suffix(
                &fixture.roots.config.join("keymap.json"),
                &format!(".{STABLE_REFRESH_BACKUP_SUFFIX}")
            )
            .exists()
        );
        assert_eq!(
            count_rows(&fixture.roots.data.join("threads/threads.db"), "threads"),
            1
        );
        assert!(fixture.marker().completed.contains(STABLE_REFRESH_KEY));
    }

    #[test]
    fn database_created_after_a_failed_copy_is_replaced_on_retry() {
        let fixture = Fixture::new();
        let relative = database_relative_path("global");
        create_database(
            &fixture.roots.legacy_data.join(&relative),
            &[
                "CREATE TABLE items (id INTEGER)",
                "INSERT INTO items VALUES (1)",
            ],
        );
        let key = database_key("global");
        let mut marker = Marker::default();
        marker.attempts.insert(key.clone(), 1);
        fs::create_dir_all(&fixture.roots.data).expect("create data dir");
        marker
            .save(&fixture.roots.data.join(MARKER_FILE_NAME))
            .expect("save marker");
        // What Lathe opens when it starts without the import.
        let destination = fixture.roots.data.join(&relative);
        create_database(&destination, &["CREATE TABLE items (id INTEGER)"]);

        fixture.import();

        assert!(fixture.marker().completed.contains(&key));
        assert_eq!(count_rows(&destination, "items"), 1);
        assert!(
            path_with_suffix(&destination, &format!(".{FAILED_IMPORT_BACKUP_SUFFIX}")).exists()
        );
    }

    #[test]
    fn copies_databases_while_another_connection_is_writing() {
        let fixture = Fixture::new();
        let relative = database_relative_path("stable");
        let source = fixture.roots.legacy_data.join(&relative);
        create_database(
            &source,
            &[
                "PRAGMA journal_mode = WAL",
                "CREATE TABLE items (value TEXT)",
                "INSERT INTO items VALUES ('committed')",
            ],
        );
        let writer =
            sqlite::Database::open(&source, sqlite::OpenMode::ReadWrite).expect("open writer");
        writer
            .execute("INSERT INTO items VALUES ('also committed')")
            .expect("insert");
        writer.execute("BEGIN").expect("begin");
        writer
            .execute("INSERT INTO items VALUES ('uncommitted')")
            .expect("insert");
        let before = legacy_snapshot(&fixture);

        fixture.import();

        assert_eq!(legacy_snapshot(&fixture), before);
        assert_eq!(count_rows(&fixture.roots.data.join(&relative), "items"), 2);
        writer.execute("ROLLBACK").expect("rollback");
    }

    #[test]
    fn concurrent_imports_copy_once() {
        let fixture = Fixture::new();
        fixture.write_legacy_config("settings.json", "{}");
        fs::create_dir_all(&fixture.roots.data).expect("create");
        let lock = ImportLock::acquire(&fixture.roots.data.join(LOCK_FILE_NAME))
            .expect("lock")
            .expect("acquired");

        let report = fixture.import();

        assert!(!fixture.roots.config.join("settings.json").exists());
        assert!(
            report
                .messages
                .iter()
                .any(|(_, message)| message.contains("another Lathe instance"))
        );
        drop(lock);
        fixture.import();
        assert!(fixture.roots.config.join("settings.json").exists());
    }

    #[test]
    fn missing_account_directories_are_expired() {
        let fixture = Fixture::new();
        let present = fixture.roots.legacy_data.join("ai_accounts/gemini/present");
        fs::create_dir_all(&present).expect("create");
        let index = serde_json::json!({
            "accounts": [
                {"display_name": "present", "config_dir": present, "state": "authenticated"},
                {"display_name": "gone", "config_dir": "/nonexistent/lathe-test", "state": "authenticated"},
                {"display_name": "never signed in", "config_dir": "/nonexistent/other", "state": "created"},
            ],
            "active_id": null,
        });
        fixture.write_legacy_config("ai_accounts.json", &index.to_string());

        let report = fixture.import();

        assert_eq!(report.accounts_needing_sign_in, vec!["gone".to_string()]);
        let copied: serde_json::Value = serde_json::from_slice(
            &fs::read(fixture.roots.config.join("ai_accounts.json")).expect("read"),
        )
        .expect("parse");
        let states: Vec<_> = copied["accounts"]
            .as_array()
            .expect("accounts")
            .iter()
            .map(|account| account["state"].as_str().expect("state").to_string())
            .collect();
        assert_eq!(states, ["authenticated", "expired", "created"]);
        assert_eq!(
            copied["accounts"][0]["config_dir"],
            serde_json::json!(present)
        );
    }

    fn shared_database_statements() -> Vec<&'static str> {
        vec![
            "CREATE TABLE migrations (domain TEXT, step INTEGER, migration TEXT)",
            "CREATE TABLE workspaces (workspace_id INTEGER, collab_account_id TEXT, identity_paths TEXT)",
            "INSERT INTO workspaces VALUES (1, NULL, '/project')",
            "INSERT INTO migrations VALUES ('WorkspaceDb', 0, 'CREATE TABLE workspaces (workspace_id INTEGER);')",
            "INSERT INTO migrations VALUES ('WorkspaceDb', 1, 'ALTER TABLE
  workspaces
ADD
  COLUMN collab_account_id TEXT DEFAULT NULL;')",
            "INSERT INTO migrations VALUES ('WorkspaceDb', 2, 'ALTER TABLE workspaces ADD COLUMN identity_paths TEXT;')",
            "INSERT INTO migrations VALUES ('WorkspaceDb', 3, 'ALTER TABLE workspaces ADD COLUMN native_window_state BLOB;')",
            "INSERT INTO migrations VALUES ('ThreadMetadataDb', 0, 'CREATE TABLE sidebar_threads (id TEXT);')",
            "INSERT INTO migrations VALUES ('ThreadMetadataDb', 1, 'ALTER TABLE sidebar_threads ADD COLUMN workspace_id INTEGER;')",
            "INSERT INTO migrations VALUES ('RecentFilesDb', 0, 'CREATE TABLE recent_files (path TEXT);')",
        ]
    }

    #[test]
    fn repairs_zed_database_after_copying_it() {
        let fixture = Fixture::new();
        let relative = database_relative_path("stable");
        let zed_database = fixture.roots.legacy_data.join(&relative);
        create_database(&zed_database, &shared_database_statements());

        let report = fixture.import();

        // Lathe's copy keeps Lathe's rows.
        let lathe_database = fixture.roots.data.join(&relative);
        assert_eq!(migration_rows(&lathe_database, "WorkspaceDb").len(), 4);
        // Zed's copy is back to upstream's list, renumbered, with data intact.
        assert_eq!(
            migration_rows(&zed_database, "WorkspaceDb"),
            vec![
                (
                    0,
                    "CREATE TABLE workspaces (workspace_id INTEGER);".to_string()
                ),
                (
                    1,
                    "ALTER TABLE workspaces ADD COLUMN identity_paths TEXT;".to_string()
                ),
                (
                    2,
                    "ALTER TABLE workspaces ADD COLUMN native_window_state BLOB;".to_string()
                ),
            ]
        );
        assert_eq!(migration_rows(&zed_database, "ThreadMetadataDb").len(), 1);
        assert_eq!(migration_rows(&zed_database, "RecentFilesDb").len(), 1);
        assert_eq!(count_rows(&zed_database, "workspaces"), 1);
        let backup = path_with_suffix(&zed_database, ".before-lathe-repair");
        assert_eq!(migration_rows(&backup, "WorkspaceDb").len(), 4);
        assert!(
            fixture
                .marker()
                .completed
                .contains(&repair_key("stable"))
        );
        assert!(
            report
                .messages
                .iter()
                .any(|(_, message)| message.contains("repaired stock Zed's stable database"))
        );
    }

    #[test]
    fn repair_leaves_healthy_zed_database_alone() {
        let fixture = Fixture::new();
        let zed_database = fixture
            .roots
            .legacy_data
            .join(database_relative_path("stable"));
        create_database(
            &zed_database,
            &[
                "CREATE TABLE migrations (domain TEXT, step INTEGER, migration TEXT)",
                "INSERT INTO migrations VALUES ('WorkspaceDb', 0, 'CREATE TABLE workspaces (workspace_id INTEGER);')",
            ],
        );
        let before = legacy_snapshot(&fixture);

        fixture.import();

        assert_eq!(legacy_snapshot(&fixture), before);
    }

    #[test]
    fn each_channel_copies_and_repairs_only_its_own_scope() {
        let fixture = Fixture::new();
        let beta = fixture
            .roots
            .legacy_data
            .join(database_relative_path("beta"));
        let stable = fixture
            .roots
            .legacy_data
            .join(database_relative_path("stable"));
        let preview = fixture
            .roots
            .legacy_data
            .join(database_relative_path("preview"));
        create_database(&beta, &shared_database_statements());
        create_database(&stable, &shared_database_statements());
        create_database(&preview, &shared_database_statements());
        let lathe_copy = |scope| fixture.roots.data.join(database_relative_path(scope));

        // An older Lathe Stable still uses Zed's 0-stable, so Beta leaves it
        // alone.
        fixture.import_as("beta");
        assert_eq!(migration_rows(&lathe_copy("beta"), "WorkspaceDb").len(), 4);
        assert_eq!(migration_rows(&lathe_copy("preview"), "WorkspaceDb").len(), 4);
        assert!(!lathe_copy("stable").exists());
        assert_eq!(migration_rows(&stable, "WorkspaceDb").len(), 4);
        assert_eq!(migration_rows(&preview, "WorkspaceDb").len(), 3);
        // Zed never opens 0-beta, and an older Lathe Beta may still use it.
        assert_eq!(migration_rows(&beta, "WorkspaceDb").len(), 4);

        fixture.import_as("stable");
        assert_eq!(migration_rows(&lathe_copy("stable"), "WorkspaceDb").len(), 4);
        assert_eq!(migration_rows(&stable, "WorkspaceDb").len(), 3);
        assert_eq!(migration_rows(&beta, "WorkspaceDb").len(), 4);
    }

    #[test]
    fn stable_redoes_stable_scope_imported_by_beta_1_11_0() {
        let fixture = Fixture::new();
        let stable = fixture
            .roots
            .legacy_data
            .join(database_relative_path("stable"));
        create_database(&stable, &shared_database_statements());
        let lathe_copy = fixture.roots.data.join(database_relative_path("stable"));
        create_database(&lathe_copy, &["CREATE TABLE stale (id INTEGER)"]);
        Marker {
            completed: ["db/0-stable", "repair_zed_db/0-stable"]
                .into_iter()
                .map(String::from)
                .collect(),
            ..Marker::default()
        }
        .save(&fixture.roots.data.join(MARKER_FILE_NAME))
        .expect("save marker");

        fixture.import_as("stable");

        assert_eq!(migration_rows(&lathe_copy, "WorkspaceDb").len(), 4);
        assert_eq!(migration_rows(&stable, "WorkspaceDb").len(), 3);
    }

    fn linux_roots(path: &Path) -> Roots {
        Roots {
            legacy_config: path.join("config/zed"),
            legacy_data: path.join("share/zed"),
            config: path.join("config/lathe"),
            data: path.join("share").join(paths::LINUX_DATA_DIR_NAME),
        }
    }

    #[test]
    fn relocate_linux_data_leaves_the_installation_behind() {
        let root = tempfile::tempdir().expect("temp dir");
        let roots = linux_roots(root.path());
        let old = root.path().join("share").join(paths::APP_NAME_LOWERCASE);
        write(&old.join("libexec/lathe-editor"), "binary");
        write(&old.join("bin/lathe"), "cli");
        write(&old.join("db/0-beta/db.sqlite"), "db");
        write(&old.join("ai_accounts/claude-acp/one/settings.json"), "{}");
        write(&old.join(MARKER_FILE_NAME), "{}");
        let account_dir = old.join("ai_accounts/claude-acp/one");
        let elsewhere = root.path().join("profiles/work");
        write(
            &roots.config.join("ai_accounts.json"),
            &serde_json::json!({
                "accounts": [
                    { "id": "one", "config_dir": account_dir },
                    { "id": "two", "config_dir": elsewhere },
                ]
            })
            .to_string(),
        );

        let mut report = ImportReport::default();
        relocate_linux_data(&roots, &mut report).expect("relocate");

        assert!(old.join("libexec/lathe-editor").exists());
        assert!(old.join("bin/lathe").exists());
        assert!(!old.join("db").exists());
        assert!(!old.join(MARKER_FILE_NAME).exists());
        assert!(roots.data.join("db/0-beta/db.sqlite").exists());
        assert!(roots.data.join(MARKER_FILE_NAME).exists());
        let index: serde_json::Value = serde_json::from_slice(
            &fs::read(roots.config.join("ai_accounts.json")).expect("read index"),
        )
        .expect("parse index");
        assert_eq!(
            index["accounts"][0]["config_dir"],
            serde_json::json!(roots.data.join("ai_accounts/claude-acp/one"))
        );
        assert_eq!(
            index["accounts"][1]["config_dir"],
            serde_json::json!(elsewhere)
        );
    }

    #[test]
    fn relocate_linux_data_ignores_a_bare_installation() {
        let root = tempfile::tempdir().expect("temp dir");
        let roots = linux_roots(root.path());
        let old = root.path().join("share").join(paths::APP_NAME_LOWERCASE);
        write(&old.join("libexec/lathe-editor"), "binary");
        write(&old.join("node/bin/node"), "x");

        let mut report = ImportReport::default();
        relocate_linux_data(&roots, &mut report).expect("relocate");

        assert!(old.join("node/bin/node").exists());
        assert!(!roots.data.exists());
    }

    #[test]
    fn legacy_sibling_uses_zeds_name() {
        let lathe = Path::new("/data").join(paths::APP_NAME);
        let lowercase = Path::new("/config").join(paths::APP_NAME_LOWERCASE);
        if paths::APP_NAME == LEGACY_APP_NAME {
            assert_eq!(legacy_sibling(&lathe), None);
        } else {
            assert_eq!(legacy_sibling(&lathe), Some(PathBuf::from("/data/Zed")));
            assert_eq!(
                legacy_sibling(&lowercase),
                Some(PathBuf::from("/config/zed"))
            );
            assert_eq!(
                legacy_sibling(&Path::new("/share").join(paths::LINUX_DATA_DIR_NAME)),
                Some(PathBuf::from("/share/zed"))
            );
        }
        assert_eq!(legacy_sibling(Path::new("/custom/dir")), None);
    }

    /// New upstream code that hardcodes one of Zed's paths merges cleanly and
    /// silently uses Zed's folders. This fails on any such hit that isn't in
    /// the reviewed allowlist below. Run it after every upstream sync.
    #[test]
    fn no_new_hardcoded_zed_paths() {
        const PATTERNS: &[&str] = &[
            "Application Support/Zed",
            "Library/Logs/Zed",
            "Library/Caches/Zed",
            ".config/zed",
            ".local/share/zed",
            "join(\"Zed\")",
            "join(\"zed\")",
            "\"zed:url=",
            "\"Zed.log",
        ];
        // `path:pattern` pairs reviewed and known not to touch local user data:
        // comments, test fixtures, remote-host paths and temp dirs.
        const ALLOWED: &[&str] = &ALLOWED_HARDCODED_ZED_PATHS;

        let crates_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut hits = BTreeSet::new();
        let mut stack = vec![crates_dir.clone()];
        while let Some(directory) = stack.pop() {
            for entry in fs::read_dir(&directory).expect("read crates").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if path.file_name().is_some_and(|name| name != "target") {
                        stack.push(path);
                    }
                    continue;
                }
                if path.extension().is_none_or(|extension| extension != "rs") {
                    continue;
                }
                let Ok(contents) = fs::read_to_string(&path) else {
                    continue;
                };
                let relative = path
                    .strip_prefix(&crates_dir)
                    .expect("under crates")
                    .to_string_lossy()
                    .replace('\\', "/");
                if relative.starts_with("lathe_data_import/") {
                    continue;
                }
                for line in contents.lines() {
                    let code = line.trim_start();
                    if code.starts_with("//") {
                        continue;
                    }
                    for pattern in PATTERNS {
                        if code.contains(pattern) {
                            hits.insert(format!("{relative}:{pattern}"));
                        }
                    }
                }
            }
        }

        let allowed: BTreeSet<String> = ALLOWED.iter().map(|entry| entry.to_string()).collect();
        let unexpected: Vec<_> = hits.difference(&allowed).collect();
        assert!(
            unexpected.is_empty(),
            "New hardcoded Zed paths. Route them through `paths` (APP_NAME), or add them to \
             ALLOWED_HARDCODED_ZED_PATHS if they don't touch local user data:\n{unexpected:#?}"
        );
    }

    const ALLOWED_HARDCODED_ZED_PATHS: [&str; 10] = [
        // Temp directories for extracting and testing update archives.
        "auto_update/src/auto_update.rs:join(\"zed\")",
        // `--user-data-dir` help text, and the Flatpak `bin/zed` launcher.
        "cli/src/main.rs:Application Support/Zed",
        "cli/src/main.rs:join(\"zed\")",
        // Test fixtures for paths inside a container.
        "prettier/src/prettier.rs:.config/zed",
        "settings/src/settings_file.rs:.config/zed",
        // Remote hosts, not local user data.
        "remote/src/transport/docker.rs:.local/share/zed",
        "remote/src/transport/ssh.rs:.local/share/zed",
        "remote/src/transport/wsl.rs:.local/share/zed",
        "remote_server/tests/command.rs:.local/share/zed",
        // Temp project directories named `zed`.
        "zed/src/visual_test_runner.rs:join(\"zed\")",
    ];
}
