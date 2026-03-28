use std::env;
use std::path::{Path, PathBuf};

// Return the letter of the volume to use for tests.
// Priority:
// 1. CI: use the current working directory's drive letter.
// 2. Local: use the R: ramdisk if available.
// 3. Fallback: use the system drive (typically C:).
pub fn test_volume_letter() -> String {
    if env::var_os("CI").is_some() {
        if let Ok(cwd) = env::current_dir() {
            let s = cwd.display().to_string();
            // Expect a Windows path like "C:\..." - take the first
            // character before ':' as the drive letter.
            if s.len() >= 2 && s.chars().nth(1) == Some(':') {
                return s.chars().next().unwrap().to_string();
            }
        }
    }

    // Prefer the ramdisk letter used in local dev machines.
    // Fall back to the system drive if R: is unavailable.
    let ramdisk = Path::new("R:\\");
    if ramdisk.exists() {
        return "R".to_string();
    }

    env::var("SystemDrive")
        .ok()
        .and_then(|s| s.chars().next())
        .unwrap_or('C')
        .to_string()
}

pub struct TempDirGuard(pub PathBuf);

impl TempDirGuard {
    pub fn new<P: AsRef<Path>>(path: P) -> std::io::Result<Self> {
        let p = path.as_ref().to_path_buf();
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p)?;
        Ok(TempDirGuard(p))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
