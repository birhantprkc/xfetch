//! Detects how the running binary was installed.
//!
//! The updater uses this to decide whether replacing the file is safe. The
//! check is intentionally conservative: an unrecognized path is never
//! overwritten automatically.

use std::path::{Path, PathBuf};

/// Where the current executable appears to come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallSource {
    /// Installed with `cargo install` (`~/.cargo/bin`).
    Cargo,
    /// Owned by a package manager (`/usr/bin`, Homebrew, ...).
    PackageManager,
    /// Installed by `install-prebuilt.sh` (`~/.local/bin`).
    Prebuilt,
    /// A local build or an unrecognized location.
    Unknown,
}

impl InstallSource {
    /// Short label used in reports.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Cargo => "cargo install",
            Self::PackageManager => "package manager",
            Self::Prebuilt => "prebuilt install",
            Self::Unknown => "local build or unknown",
        }
    }
}

/// Classifies `executable` using the real home directory.
pub fn detect(executable: &Path) -> InstallSource {
    detect_with_home(executable, dirs::home_dir().as_deref())
}

/// Classifies `executable` against an explicit home directory (testable).
pub fn detect_with_home(executable: &Path, home: Option<&Path>) -> InstallSource {
    if let Some(home) = home
        && executable.starts_with(home.join(".cargo").join("bin"))
    {
        return InstallSource::Cargo;
    }

    if is_package_manager_path(executable) {
        return InstallSource::PackageManager;
    }

    if let Some(home) = home
        && executable.starts_with(home.join(".local").join("bin"))
    {
        return InstallSource::Prebuilt;
    }

    InstallSource::Unknown
}

/// Paths owned by common package managers.
#[cfg(unix)]
fn is_package_manager_path(executable: &Path) -> bool {
    ["/usr/bin", "/bin", "/opt/homebrew", "/usr/local/Cellar"]
        .iter()
        .any(|prefix| executable.starts_with(prefix))
}

/// Windows installs are never treated as package-managed here.
#[cfg(not(unix))]
fn is_package_manager_path(_executable: &Path) -> bool {
    false
}

/// Resolves a home-relative path for tests and reports.
#[allow(dead_code)]
pub fn home_path(home: &Path, rest: &str) -> PathBuf {
    home.join(rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(path: &str, home: &str) -> InstallSource {
        detect_with_home(Path::new(path), Some(Path::new(home)))
    }

    #[test]
    fn detects_cargo_installs() {
        assert_eq!(
            classify("/home/u/.cargo/bin/xfetch", "/home/u"),
            InstallSource::Cargo
        );
    }

    #[test]
    fn detects_prebuilt_installs() {
        assert_eq!(
            classify("/home/u/.local/bin/xfetch", "/home/u"),
            InstallSource::Prebuilt
        );
    }

    #[test]
    fn detects_local_builds() {
        assert_eq!(
            classify("/home/u/repos/xfetch/target/release/xfetch", "/home/u"),
            InstallSource::Unknown
        );
    }

    #[cfg(unix)]
    #[test]
    fn detects_package_manager_paths() {
        assert_eq!(
            classify("/usr/bin/xfetch", "/home/u"),
            InstallSource::PackageManager
        );
        assert_eq!(
            classify("/bin/xfetch", "/home/u"),
            InstallSource::PackageManager
        );
        assert_eq!(
            classify("/opt/homebrew/bin/xfetch", "/home/u"),
            InstallSource::PackageManager
        );
    }

    #[test]
    fn missing_home_falls_back_to_unknown() {
        assert_eq!(
            detect_with_home(Path::new("/random/xfetch"), None),
            InstallSource::Unknown
        );
    }
}
