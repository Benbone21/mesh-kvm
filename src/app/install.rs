//! Putting the exe where the login scripts expect it, even while an older copy is running there.
//! Windows won't overwrite a running exe but will rename it, so the old copy is moved aside first.

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

/// True when `current` is not the installed exe and not a byte-for-byte copy of it.
pub fn needs_install(current: &Path, installed: &Path) -> bool {
    if current == installed {
        return false;
    }
    match (std::fs::read(current), std::fs::read(installed)) {
        (Ok(a), Ok(b)) => a != b,
        _ => true,
    }
}

/// Deletes the parked copy from an earlier update. It fails while that copy still runs; that's
/// fine, a later start tries again.
pub fn clear_stale(installed: &Path) {
    let _ = std::fs::remove_file(aside_path(installed));
}

/// Makes `installed` a copy of `current`, moving any existing (possibly running) exe aside first.
/// If the copy fails, the old exe goes back.
pub fn replace_exe(current: &Path, installed: &Path) -> Result<(), String> {
    if let Some(dir) = installed.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("Couldn't create {}: {e}", dir.display()))?;
    }
    clear_stale(installed);
    let aside = aside_path(installed);
    let parked = installed.exists();
    if parked {
        std::fs::rename(installed, &aside).map_err(|e| {
            format!("Couldn't move the installed {} aside: {e}", installed.display())
        })?;
    }
    if let Err(e) = std::fs::copy(current, installed) {
        if parked {
            let _ = std::fs::remove_file(installed);
            let _ = std::fs::rename(&aside, installed);
        }
        return Err(format!("Couldn't copy {} to {}: {e}", current.display(), installed.display()));
    }
    Ok(())
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
    fn running_the_installed_exe_needs_no_install() {
        let dir = temp("same");
        let exe = dir.join("mesh-kvm.exe");
        fs::write(&exe, "a").unwrap();
        assert!(!needs_install(&exe, &exe));
    }

    #[test]
    fn a_different_exe_needs_install_unless_it_is_identical() {
        let dir = temp("differs");
        let (cur, installed) = (dir.join("new.exe"), dir.join("mesh-kvm.exe"));
        fs::write(&cur, "new").unwrap();
        assert!(needs_install(&cur, &installed), "nothing installed yet");
        fs::write(&installed, "old").unwrap();
        assert!(needs_install(&cur, &installed));
        fs::write(&installed, "new").unwrap();
        assert!(!needs_install(&cur, &installed), "same bytes");
    }

    #[test]
    fn replace_moves_the_old_exe_aside_and_copies_the_new_one() {
        let dir = temp("replace");
        let (cur, installed) = (dir.join("new.exe"), dir.join("mesh-kvm.exe"));
        fs::write(&cur, "new").unwrap();
        fs::write(&installed, "old").unwrap();
        replace_exe(&cur, &installed).unwrap();
        assert_eq!(fs::read_to_string(&installed).unwrap(), "new");
        assert_eq!(fs::read_to_string(aside_path(&installed)).unwrap(), "old");
    }

    #[test]
    fn replace_clears_a_stale_aside_copy_first() {
        let dir = temp("stale");
        let (cur, installed) = (dir.join("new.exe"), dir.join("mesh-kvm.exe"));
        fs::write(&cur, "new").unwrap();
        fs::write(&installed, "old").unwrap();
        fs::write(aside_path(&installed), "older").unwrap();
        replace_exe(&cur, &installed).unwrap();
        assert_eq!(fs::read_to_string(aside_path(&installed)).unwrap(), "old");
    }

    #[test]
    fn replace_works_with_nothing_installed() {
        let dir = temp("fresh");
        let (cur, installed) = (dir.join("new.exe"), dir.join("mesh-kvm.exe"));
        fs::write(&cur, "new").unwrap();
        replace_exe(&cur, &installed).unwrap();
        assert_eq!(fs::read_to_string(&installed).unwrap(), "new");
    }

    #[test]
    fn a_failed_copy_puts_the_old_exe_back() {
        let dir = temp("restore");
        let installed = dir.join("mesh-kvm.exe");
        fs::write(&installed, "old").unwrap();
        let missing = dir.join("nope.exe");
        assert!(replace_exe(&missing, &installed).is_err());
        assert_eq!(fs::read_to_string(&installed).unwrap(), "old");
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
