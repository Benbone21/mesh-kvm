//! Where the Windows exe is installed. `mesh-kvm-setup.exe` puts it there, and the updater keeps it
//! there. Windows won't overwrite a running exe but will rename it, so an update moves the old copy
//! aside first.

#![cfg_attr(not(target_os = "windows"), allow(dead_code))] // Only the Windows build installs.

use std::path::{Path, PathBuf};

/// Where the old exe is parked while its replacement takes the installed name.
pub fn aside_path(installed: &Path) -> PathBuf {
    let mut name = installed.as_os_str().to_owned();
    name.push(".old");
    PathBuf::from(name)
}

/// `%LOCALAPPDATA%\mesh-kvm\mesh-kvm.exe` (falling back to `%APPDATA%`).
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn installed_exe() -> Option<PathBuf> {
    let base = std::env::var("LOCALAPPDATA").or_else(|_| std::env::var("APPDATA")).ok()?;
    Some(PathBuf::from(base).join("mesh-kvm").join("mesh-kvm.exe"))
}

/// True when `current` is the installed exe, however the two are spelt.
pub fn is_installed_copy(current: &Path, installed: &Path) -> bool {
    current == installed || matches!((current.canonicalize(), installed.canonicalize()), (Ok(a), Ok(b)) if a == b)
}

/// True when this exe is the installed one. Only that copy writes launchers or updates itself:
/// one run from anywhere else, such as Downloads, leaves the installed copy alone.
pub fn runs_from_installed() -> bool {
    let (Ok(exe), Some(installed)) = (std::env::current_exe(), installed_exe()) else { return false };
    is_installed_copy(&exe, &installed)
}

/// Deletes the parked copy from an earlier update. It fails while that copy still runs; that's
/// fine, a later start tries again.
pub fn clear_stale(installed: &Path) {
    let _ = std::fs::remove_file(aside_path(installed));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mesh-kvm-install-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn old_copy_sits_beside_the_installed_exe() {
        assert_eq!(aside_path(Path::new("C:/a/mesh-kvm.exe")), PathBuf::from("C:/a/mesh-kvm.exe.old"));
    }

    #[test]
    fn the_installed_exe_is_known_however_it_is_spelt() {
        let dir = temp("same");
        let exe = dir.join("mesh-kvm.exe");
        fs::write(&exe, "a").unwrap();
        assert!(is_installed_copy(&exe, &exe));
        assert!(is_installed_copy(&dir.join(".").join("mesh-kvm.exe"), &exe));
    }

    #[test]
    fn a_copy_somewhere_else_is_not_the_installed_one_even_with_the_same_bytes() {
        let dir = temp("elsewhere");
        let (download, installed) = (dir.join("Downloads-mesh-kvm.exe"), dir.join("mesh-kvm.exe"));
        fs::write(&download, "same").unwrap();
        fs::write(&installed, "same").unwrap();
        assert!(!is_installed_copy(&download, &installed));
        assert!(!is_installed_copy(&download, &dir.join("nothing-here.exe")));
    }

    #[test]
    fn clear_stale_removes_the_aside_copy() {
        let dir = temp("clear");
        let installed = dir.join("mesh-kvm.exe");
        fs::write(aside_path(&installed), "old").unwrap();
        clear_stale(&installed);
        assert!(!aside_path(&installed).exists());
    }
}
