//! One bot process per token on a machine.
//!
//! Two processes logged in with the same token both receive every command:
//! one acknowledges it, the other logs "Unknown interaction", and a `/stop`
//! may be answered by the process that isn't playing. An exclusive `flock` on
//! a file named after a hash of the token makes a second process exit at
//! startup instead. The lock is released when the process exits, however it
//! exits. (In ECS one task per service is guaranteed by the deployment.)

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::AppError;

/// Held for the life of the process.
pub struct InstanceLock {
    _file: File,
}

pub fn lock_path(dir: &Path, token: &str) -> PathBuf {
    let digest = Sha256::digest(token.as_bytes());
    let id: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    dir.join(format!("tatarbot-discord-{id}.lock"))
}

/// `Ok(None)` when no lock file can be created (e.g. a read-only filesystem):
/// a warning, not a reason to refuse to start.
pub fn acquire(dir: &Path, token: &str) -> Result<Option<InstanceLock>, AppError> {
    let path = lock_path(dir, token);
    let mut file = match OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
    {
        Ok(file) => file,
        Err(err) => {
            tracing::warn!(path = %path.display(), %err, "no single-instance lock");
            return Ok(None);
        }
    };
    if !try_lock(&file) {
        let mut pid = String::new();
        let _ = file.read_to_string(&mut pid);
        return Err(AppError::AlreadyRunning(pid.trim().to_owned()));
    }
    let _ = file.set_len(0);
    let _ = file.rewind();
    let _ = write!(file, "{}", std::process::id());
    Ok(Some(InstanceLock { _file: file }))
}

#[cfg(unix)]
fn try_lock(file: &File) -> bool {
    use std::os::fd::AsRawFd;
    // SAFETY: flock on a valid, owned file descriptor; no memory is shared.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

#[cfg(not(unix))]
fn try_lock(_: &File) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_instance_with_the_same_token_is_refused() {
        let dir = tempfile::tempdir().unwrap();

        let first = acquire(dir.path(), "token-a").unwrap().expect("lock");
        let second = acquire(dir.path(), "token-a");
        assert!(
            matches!(&second, Err(AppError::AlreadyRunning(pid)) if *pid == std::process::id().to_string()),
            "{:?}",
            second.err()
        );
        // Another token is another bot.
        assert!(acquire(dir.path(), "token-b").unwrap().is_some());

        drop(first);
        assert!(acquire(dir.path(), "token-a").unwrap().is_some());
    }

    #[test]
    fn the_lock_file_name_does_not_contain_the_token() {
        let path = lock_path(Path::new("/tmp"), "secret.token.value");
        assert!(!path.to_string_lossy().contains("secret"));
    }

    #[test]
    fn an_unwritable_directory_is_only_a_warning() {
        assert!(
            acquire(Path::new("/nonexistent/dir"), "t")
                .unwrap()
                .is_none()
        );
    }
}
