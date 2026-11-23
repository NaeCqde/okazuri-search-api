//! Keeping the pages of the database file in memory (`[search] pin_db`).
//!
//! The file is mapped and the map is locked (`mlock`): the system never takes its pages out of the page cache, so a search does not wait for
//! the disk, however much memory the other programs of the machine want. It is what `vmtouch -l search.db` does, built in. Only the main file is
//! locked, not its WAL. Nothing is read through the map: it only holds the pages.
//!
//! The file grows with every work that is saved, so its size is looked at every minute, and when it changed the new size is locked (the old
//! lock is let go after that, so there is no moment with nothing locked). Without the right to lock that much memory (in Docker:
//! `cap_add: IPC_LOCK` and `ulimits: memlock: -1`) the server only warns, once for each reason, and tries again at the next change.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// How often the size of the file is looked at.
const CHECK_EVERY: Duration = Duration::from_secs(60);

/// What is locked now: the size of the file when it was mapped, and the map.
#[derive(Default)]
struct Pinned {
    len: u64,
    #[cfg(unix)]
    map: Option<memmap2::Mmap>,
}

/// Whether the file has to be locked (again): it is not locked yet, or its size is not the one that was locked.
fn needs_pin(current: u64, pinned: u64) -> bool {
    current > 0 && current != pinned
}

#[cfg(unix)]
fn lock_file(path: &Path) -> std::io::Result<(u64, memmap2::Mmap)> {
    let file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    // SAFETY: the map is never read, it only holds the pages (SQLite writes the file meanwhile: that matters only to a program that reads the map)
    let map = unsafe { memmap2::Mmap::map(&file)? };
    map.lock()?;

    Ok((len, map))
}

#[cfg(unix)]
impl Pinned {
    /// Looks at the file and locks it if the size changed. Returns the size that is locked now when that is new.
    fn refresh(&mut self, path: &Path) -> std::io::Result<Option<u64>> {
        let current = std::fs::metadata(path)?.len();
        if !needs_pin(current, self.len) {
            return Ok(None);
        }

        let (len, map) = lock_file(path)?;
        // the new lock is held before the old one is let go
        self.map = Some(map);
        self.len = len;

        Ok(Some(len))
    }
}

/// Locks the file in memory for as long as the server runs.
#[cfg(unix)]
pub async fn run(path: PathBuf) {
    let mut pinned = Pinned::default();
    // the last error that was logged: the same one is not logged every minute
    let mut warned: Option<String> = None;

    loop {
        // locking reads the whole file, which takes a while: not on the threads of the server
        let looked_at = path.clone();
        let moved = tokio::task::spawn_blocking(move || {
            let result = pinned.refresh(&looked_at);
            (pinned, result)
        })
        .await;

        let Ok((back, result)) = moved else {
            log::warn!("pin_db: the task that locks the file stopped");
            return;
        };
        pinned = back;

        match result {
            Ok(Some(len)) => {
                warned = None;
                log::info!("pin_db: {} MiB of {} are locked in memory", len / (1024 * 1024), path.display());
            }
            Ok(None) => {}
            Err(e) => {
                let message = e.to_string();
                if warned.as_deref() != Some(message.as_str()) {
                    log::warn!(
                        "pin_db: cannot lock {} in memory ({message}); the right to lock memory is needed (Docker: cap_add IPC_LOCK and ulimits memlock -1)",
                        path.display()
                    );
                    warned = Some(message);
                }
            }
        }

        tokio::time::sleep(CHECK_EVERY).await;
    }
}

#[cfg(not(unix))]
pub async fn run(path: PathBuf) {
    log::warn!("pin_db: {} cannot be locked in memory on this system, only Unix can", path.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_is_locked_again_only_when_its_size_changed() {
        assert!(needs_pin(100, 0), "not locked yet");
        assert!(needs_pin(200, 100), "it grew");
        assert!(!needs_pin(100, 100), "the same size");
        assert!(!needs_pin(0, 0), "an empty file has nothing to lock");
    }

    #[cfg(unix)]
    #[test]
    fn a_file_is_locked_and_locked_again_when_it_grows() {
        let path = std::env::temp_dir().join(format!("okazuri-pin-test-{}", std::process::id()));
        std::fs::write(&path, vec![1u8; 64 * 1024]).unwrap();

        let mut pinned = Pinned::default();
        match pinned.refresh(&path) {
            Ok(first) => {
                assert_eq!(first, Some(64 * 1024));
                assert_eq!(pinned.refresh(&path).unwrap(), None, "the same size is not locked again");

                std::fs::write(&path, vec![1u8; 128 * 1024]).unwrap();
                assert_eq!(pinned.refresh(&path).unwrap(), Some(128 * 1024));
            }
            // a machine that does not let this user lock memory (a small RLIMIT_MEMLOCK): the server only warns, which is the behavior
            Err(e) => eprintln!("mlock is not allowed here, nothing to check: {e}"),
        }

        std::fs::remove_file(&path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_file_that_is_not_there_is_an_error_not_a_panic() {
        let mut pinned = Pinned::default();

        assert!(pinned.refresh(Path::new("/nonexistent/okazuri-search.db")).is_err());
    }
}
