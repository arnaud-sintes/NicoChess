//! Small file helpers shared by the stores.

use anyhow::{Context, Result};
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

/// Writes a file in one step: the bytes go to a temporary file next to it, which is flushed to
/// the disk, then renamed over the target. After a power cut the file is either the old one or
/// the new one, never a half-written or empty one. (The rename alone is not enough on some
/// file systems: without the flush the renamed file can come back empty.)
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    let mut file = File::create(&tmp).with_context(|| format!("cannot write {}", tmp.display()))?;
    file.write_all(bytes).with_context(|| format!("cannot write {}", tmp.display()))?;
    file.sync_all().with_context(|| format!("cannot flush {}", tmp.display()))?;
    drop(file);
    fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_a_file_and_leaves_no_temporary_one() {
        let dir = std::env::temp_dir().join(format!("nicochess-fsutil-{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.json");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "two");
        assert!(!dir.join("a.json.tmp").exists());
        fs::remove_dir_all(&dir).ok();
    }
}
