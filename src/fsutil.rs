//! Small file helpers shared by the stores.

use anyhow::{Context, Result};
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Writes a file in one step: the bytes go to a temporary file next to it, which is flushed to
/// the disk, then renamed over the target. After a power cut the file is either the old one or
/// the new one, never a half-written or empty one. (The rename alone is not enough on some
/// file systems: without the flush the renamed file can come back empty.)
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    // A temporary file of its own for each write: two writers of one file (a calibration set by hand and one moved by the
    // board following) would otherwise write into the same temporary file, and one could rename the half of the other.
    static WRITES: AtomicU64 = AtomicU64::new(0);
    let tmp = path.with_extension(format!("json.{}.tmp", WRITES.fetch_add(1, Ordering::Relaxed)));
    let written = (|| -> Result<()> {
        let mut file = File::create(&tmp).with_context(|| format!("cannot write {}", tmp.display()))?;
        file.write_all(bytes).with_context(|| format!("cannot write {}", tmp.display()))?;
        file.sync_all().with_context(|| format!("cannot flush {}", tmp.display()))?;
        drop(file);
        fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))
    })();
    if written.is_err() {
        // Nothing is left behind by a write that failed.
        let _ = fs::remove_file(&tmp);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writers_of_the_same_file_do_not_mix_their_bytes_and_leave_nothing_behind() {
        let dir = std::env::temp_dir().join(format!("nicochess-fsutil-race-{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("calibration.json");
        // (Every write is flushed to the disk, which is slow: few writes, so that the other tests are not held up.)
        let writers: Vec<_> = (0..4u8)
            .map(|k| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let bytes = vec![b'a' + k; 4_000];
                    for _ in 0..6 {
                        write_atomic(&path, &bytes).unwrap();
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        // The file is what one writer wrote, whole.
        let text = fs::read(&path).unwrap();
        assert_eq!(text.len(), 4_000);
        assert!(text.iter().all(|&b| b == text[0]), "bytes of two writers in one file");
        let names: Vec<_> = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["calibration.json".to_string()], "no temporary file is left: {names:?}");
        // A write that fails leaves nothing either.
        assert!(write_atomic(&dir.join("no-such-folder").join("x.json"), b"x").is_err());
        fs::remove_dir_all(&dir).ok();
    }

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
