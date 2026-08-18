//! Portable-first path resolution for POSEIDON.
//!
//! One design principle governs this crate: **the app never writes outside its
//! expected paths without permission.** Every writable location (SQLite DB,
//! logs, config overrides, cache) is resolved through [`Paths`], which picks a
//! *root* using an explicit precedence and confines everything beneath it.
//!
//! ## Resolution precedence (data root)
//!
//! 1. **Portable** - if `POSEIDON_PORTABLE_MODE=true` OR a `.portable` sentinel
//!    file sits next to the binary, the root is `./.portable/` relative to the
//!    binary. Nothing is written anywhere else. Ideal for USB drives, air-gapped
//!    environments, and throwaway trials.
//! 2. **OS-standard** - otherwise `directories::ProjectDirs` gives the platform
//!    data dir (`~/.local/share/poseidon/` on Linux, `%APPDATA%\poseidon\` on
//!    Windows, `~/Library/Application Support/poseidon/` on macOS). In the
//!    container this is where the mounted PV/PVC lands.

use std::env;
use std::path::{Path, PathBuf};

/// Env var that forces portable mode on when set to a truthy value.
pub const ENV_PORTABLE: &str = "POSEIDON_PORTABLE_MODE";
/// Env var that overrides the data root directly. Primary mechanism in the
/// container: point it at the mounted PV/PVC (`/data`) so the DB + logs land on
/// persistent storage without depending on `$HOME` resolving inside the image.
pub const ENV_DATA_DIR: &str = "POSEIDON_DATA_DIR";
/// Marker that, when present beside the binary, forces portable mode. This is the
/// same path as [`PORTABLE_DIR`] on purpose: the `.portable` data-root *directory*
/// existing beside the binary is itself the sentinel (a `.exists()` check passes
/// for the dir), so a portable install stays sticky across runs with no extra
/// sidecar file. (An earlier scheme wrote a same-named *file* here, which collided
/// with the data-root directory - see `write_portable_marker`.)
pub const PORTABLE_SENTINEL: &str = ".portable";
/// Subdirectory used as the portable root, relative to the binary.
pub const PORTABLE_DIR: &str = ".portable";

/// Resolved, confined set of writable locations for one POSEIDON run.
#[derive(Debug, Clone)]
pub struct Paths {
    data_root: PathBuf,
    portable: bool,
}

impl Paths {
    /// Resolve paths for the current process, reading the environment and the
    /// filesystem next to the binary. This is the normal entry point.
    pub fn resolve() -> Self {
        let exe = env::current_exe().ok();
        let binary_dir = exe
            .as_deref()
            .and_then(|p| p.parent())
            .map(Path::to_path_buf);
        let binary_name = exe
            .as_deref()
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .map(str::to_string);
        Self::resolve_with(
            &env_snapshot(),
            binary_dir.as_deref(),
            binary_name.as_deref(),
        )
    }

    /// Resolution core, parameterised on its inputs so it's unit-testable
    /// without touching real env vars or the real binary location.
    ///
    /// `binary_dir` is where the executable lives (used for the portable
    /// sentinel + portable root); `None` in contexts where it can't be
    /// determined, in which case the sentinel check is skipped and portable
    /// mode can still be forced via the env var (rooted at the current dir).
    /// `binary_name` is the executable's file stem: a name containing
    /// `portable` (case-insensitive) ships as the single-file portable download
    /// and flips portable mode on with no sidecar `.portable` sentinel needed.
    pub fn resolve_with(
        env: &EnvSnapshot,
        binary_dir: Option<&Path>,
        binary_name: Option<&str>,
    ) -> Self {
        let sentinel_present = binary_dir
            .map(|d| d.join(PORTABLE_SENTINEL).exists())
            .unwrap_or(false);
        // A binary whose own name contains "portable" is the single-file portable
        // build - run it and it roots under `./.portable/` beside itself, no zip or
        // sentinel file required.
        let name_portable = binary_name
            .map(|n| n.to_ascii_lowercase().contains("portable"))
            .unwrap_or(false);
        let portable = env.portable_flag || sentinel_present || name_portable;

        if portable {
            let base = binary_dir
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            return Self {
                data_root: base.join(PORTABLE_DIR),
                portable: true,
            };
        }

        // Explicit data-dir override (containers) wins over the OS-standard
        // location. Not "portable" - it's an absolute mount, not confined
        // beneath the binary.
        if let Some(dir) = env.data_dir.as_ref().filter(|s| !s.is_empty()) {
            return Self {
                data_root: PathBuf::from(dir),
                portable: false,
            };
        }

        Self {
            data_root: os_data_dir(),
            portable: false,
        }
    }

    /// Whether portable mode is active.
    pub fn is_portable(&self) -> bool {
        self.portable
    }

    /// The confining root. Every other path is beneath this.
    pub fn data_root(&self) -> &Path {
        &self.data_root
    }

    /// SQLite database file: `<root>/poseidon.db`.
    pub fn database_path(&self) -> PathBuf {
        self.data_root.join("poseidon.db")
    }

    /// Log directory: `<root>/logs/`.
    pub fn log_dir(&self) -> PathBuf {
        self.data_root.join("logs")
    }

    /// Cache directory: `<root>/cache/`.
    pub fn cache_dir(&self) -> PathBuf {
        self.data_root.join("cache")
    }

    /// Root for per-owner isolated Azure CLI token caches: `<root>/az-sessions/`.
    /// A hosted deployment mounts the data root on a PV, so each user's
    /// device-code session (`az-sessions/<owner>/`) persists across restarts and
    /// never shares the machine `~/.azure`.
    pub fn az_sessions_dir(&self) -> PathBuf {
        self.data_root.join("az-sessions")
    }

    /// Create the data root + standard subdirectories if they don't exist.
    /// Idempotent. Callers run this once at startup before any write.
    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.data_root)?;
        std::fs::create_dir_all(self.log_dir())?;
        std::fs::create_dir_all(self.cache_dir())?;
        Ok(())
    }
}

/// A read of the environment inputs path resolution depends on. Snapshotting
/// makes [`Paths::resolve_with`] a pure function of its arguments - the tests
/// build one by hand instead of mutating process env.
#[derive(Debug, Clone, Default)]
pub struct EnvSnapshot {
    pub portable_flag: bool,
    pub data_dir: Option<String>,
}

impl EnvSnapshot {
    /// Build a snapshot from the portable flag (data_dir defaults to none, set
    /// via [`Self::with_data_dir`]). Test-friendly.
    pub fn new(portable_flag: bool) -> Self {
        Self {
            portable_flag,
            data_dir: None,
        }
    }

    /// Set the explicit data-dir override.
    pub fn with_data_dir(mut self, data_dir: Option<String>) -> Self {
        self.data_dir = data_dir;
        self
    }
}

/// Read the real process environment into a snapshot.
pub fn env_snapshot() -> EnvSnapshot {
    EnvSnapshot {
        portable_flag: env::var(ENV_PORTABLE)
            .map(|v| is_truthy(&v))
            .unwrap_or(false),
        data_dir: env::var(ENV_DATA_DIR).ok().filter(|s| !s.is_empty()),
    }
}

fn is_truthy(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Directory containing the running executable, if determinable.
fn current_binary_dir() -> Option<PathBuf> {
    env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
}

/// Create the portable sentinel (`.portable`) beside the running binary, so this
/// and every future run resolves to portable mode. This is how the desktop
/// first-run flow opts into portable *before* any data is written: it's called
/// prior to `ensure_dirs`/store connection, then a fresh [`Paths::resolve`]
/// picks up the sentinel and roots everything under `./.portable/`. Returns the
/// sentinel path on success; errors if the binary directory can't be located or
/// the file can't be written (e.g. an installed, read-only program directory -
/// the caller should surface that rather than silently fall back).
pub fn enable_portable_sentinel() -> std::io::Result<PathBuf> {
    let dir = current_binary_dir().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "cannot locate the binary directory to write the portable sentinel",
        )
    })?;
    write_portable_marker(&dir)
}

/// Mark `dir` as portable by materialising the `.portable` data-root directory
/// (its presence beside the binary is itself what [`Paths::resolve_with`] reads as
/// the sentinel) and dropping a human note inside it. Split out from
/// [`enable_portable_sentinel`] so it's unit-testable against a temp dir.
///
/// Crucially the marker is the *directory*, not a same-named file: an earlier
/// scheme wrote a `.portable` file here, which then collided with the `.portable`
/// data-root directory `ensure_dirs` needs (`create_dir_all` over an existing file
/// fails with `EEXIST` / Windows os error 183). We heal that stale file if present.
fn write_portable_marker(dir: &Path) -> std::io::Result<PathBuf> {
    let root = dir.join(PORTABLE_DIR);
    if root.is_file() {
        std::fs::remove_file(&root)?;
    }
    std::fs::create_dir_all(&root)?;
    std::fs::write(
        root.join("PORTABLE.txt"),
        b"POSEIDON portable mode.\nThis .portable folder holds all data (DB, logs, cache) for the\nbinary beside it. Delete the whole folder to return to OS-standard storage.\n",
    )?;
    Ok(root)
}

/// OS-standard data directory for POSEIDON. Falls back to `./poseidon-data`
/// if the platform dirs can't be resolved (headless CI, unusual environments)
/// so the app still has somewhere confined to write.
fn os_data_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "poseidon")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("poseidon-data"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_env_flag_roots_under_binary_dir() {
        let bin = PathBuf::from("/opt/poseidon");
        let env = EnvSnapshot::new(true);
        let paths = Paths::resolve_with(&env, Some(&bin), None);
        assert!(paths.is_portable());
        assert_eq!(paths.data_root(), bin.join(".portable"));
        assert_eq!(paths.database_path(), bin.join(".portable/poseidon.db"));
        assert_eq!(paths.log_dir(), bin.join(".portable/logs"));
    }

    #[test]
    fn portable_sentinel_file_triggers_portable_mode() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(PORTABLE_SENTINEL), b"").unwrap();
        // No env flag - the sentinel alone must flip portable mode on.
        let env = EnvSnapshot::default();
        let paths = Paths::resolve_with(&env, Some(dir.path()), None);
        assert!(paths.is_portable());
        assert_eq!(paths.data_root(), dir.path().join(".portable"));
    }

    #[test]
    fn binary_named_portable_triggers_portable_mode_without_a_sentinel() {
        // The single-file portable download: the exe's own name carries the signal,
        // no sidecar `.portable` file. Case-insensitive substring.
        let bin = PathBuf::from("/downloads");
        let env = EnvSnapshot::default();
        for name in [
            "poseidon-portable",
            "POSEIDON-Portable",
            "poseidon_portable_win",
        ] {
            let paths = Paths::resolve_with(&env, Some(&bin), Some(name));
            assert!(paths.is_portable(), "{name} should be portable");
            assert_eq!(paths.data_root(), bin.join(".portable"));
        }
        // The normal installed name is NOT portable.
        let installed = Paths::resolve_with(&env, Some(&bin), Some("poseidon-app"));
        assert!(!installed.is_portable());
    }

    #[test]
    fn data_dir_override_wins_over_os_dir_but_is_not_portable() {
        let env = EnvSnapshot::default().with_data_dir(Some("/data".to_string()));
        let paths = Paths::resolve_with(&env, Some(Path::new("/usr/local/bin")), None);
        assert!(!paths.is_portable());
        assert_eq!(paths.data_root(), Path::new("/data"));
        assert_eq!(paths.database_path(), Path::new("/data/poseidon.db"));
    }

    #[test]
    fn portable_beats_data_dir_override() {
        // Portable is the strongest signal - even with a data-dir override set,
        // a portable environment stays confined beneath the binary.
        let env = EnvSnapshot::new(true).with_data_dir(Some("/data".to_string()));
        let paths = Paths::resolve_with(&env, Some(Path::new("/opt/poseidon")), None);
        assert!(paths.is_portable());
        assert_eq!(paths.data_root(), Path::new("/opt/poseidon/.portable"));
    }

    #[test]
    fn non_portable_uses_os_data_dir_not_binary_dir() {
        let bin = PathBuf::from("/usr/local/bin");
        let env = EnvSnapshot::default();
        let paths = Paths::resolve_with(&env, Some(&bin), None);
        assert!(!paths.is_portable());
        // Whatever the OS dir resolves to, it must NOT be under the binary dir.
        assert!(!paths.data_root().starts_with(&bin));
    }

    #[test]
    fn marker_creates_the_portable_dir_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        // Fresh: creates the .portable data-root directory (its presence is the
        // sentinel) + the note inside, and ensure_dirs must then succeed over it.
        let root = write_portable_marker(dir.path()).unwrap();
        assert_eq!(root, dir.path().join(".portable"));
        assert!(root.is_dir());
        assert!(root.join("PORTABLE.txt").is_file());
        let paths = Paths::resolve_with(&EnvSnapshot::default(), Some(dir.path()), None);
        assert!(
            paths.is_portable(),
            "the .portable dir must re-trigger portable"
        );
        paths
            .ensure_dirs()
            .expect("ensure_dirs must not collide with the marker");
        // Second call is a no-op, not an error.
        write_portable_marker(dir.path()).unwrap();
    }

    #[test]
    fn marker_heals_a_stale_sentinel_file_from_the_old_scheme() {
        // A binary that crashed under the old file-based scheme leaves a `.portable`
        // FILE where the data-root DIRECTORY belongs; enabling portable again must
        // replace it with the directory instead of failing with EEXIST (os 183).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".portable"), b"stale marker file").unwrap();
        assert!(dir.path().join(".portable").is_file());
        let root = write_portable_marker(dir.path()).unwrap();
        assert!(root.is_dir(), "the stale file must be replaced by the dir");
        Paths::resolve_with(&EnvSnapshot::default(), Some(dir.path()), None)
            .ensure_dirs()
            .expect("ensure_dirs must succeed after healing");
    }

    #[test]
    fn truthy_parsing() {
        assert!(is_truthy("true"));
        assert!(is_truthy("1"));
        assert!(is_truthy("YES"));
        assert!(is_truthy(" On "));
        assert!(!is_truthy("false"));
        assert!(!is_truthy("0"));
        assert!(!is_truthy(""));
    }
}
