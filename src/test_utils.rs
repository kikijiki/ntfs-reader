use std::path::{Path, PathBuf};

// Ramdisk
pub const TEST_VOLUME_LETTER: &str = "R";

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
