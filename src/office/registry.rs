//! Which Jarvo starts, and owns, the office for this directory.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// An unreadable lock, or one with no registration beside it, is trusted this
/// long so a slow startup is never mistaken for a crash.
const STALE_AFTER: Duration = Duration::from_secs(60);

// Deliberately not `Debug`: the token must never reach a log line.
#[derive(Clone, Serialize, Deserialize)]
pub struct Registration {
    pub port: u16,
    pub pid: u32,
    pub token: String,
}

/// Read `<dir>/office.json`, if it describes a live server.
pub fn find(dir: &Path) -> Option<Registration> {
    let raw = fs::read_to_string(dir.join("office.json")).ok()?;
    let registration: Registration = serde_json::from_str(&raw).ok()?;
    (is_alive(registration.pid) == Some(true)).then_some(registration)
}

/// Try to become the one that starts the office.
/// `Ok(true)` means this caller won and must now start the server;
/// `Ok(false)` means someone else won and the caller should `find` again.
pub fn claim(dir: &Path) -> io::Result<bool> {
    create_office_dir(dir)?;
    let lock = dir.join("office.lock");
    match take_lock(&lock) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if is_stale(dir, &lock) {
                reclaim(dir, &lock)
            } else {
                Ok(false)
            }
        }
        Err(error) => Err(error),
    }
}

/// Reclaim a stale lock. An advisory-locked guard makes reclamation
/// single-file: two callers that both judged the lock stale cannot both act.
/// A reclaimer holding it makes the other return `Ok(false)` and retry.
fn reclaim(dir: &Path, lock: &Path) -> io::Result<bool> {
    let guard_path = dir.join("office.reclaim");
    let Some(_guard) = ReclaimGuard::take(&guard_path)? else {
        return Ok(false);
    };
    // Another process may have installed a live lock between the caller's
    // staleness check and this guard. Judge again before removing anything.
    if !is_stale(dir, lock) {
        return Ok(false);
    }
    remove_if_present(lock)?;
    remove_if_present(&dir.join("office.json"))?;
    match take_lock(lock) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error),
    }
}

/// Exclusive while alive. The file is a stable rendezvous and is never
/// unlinked; the OS releases its advisory lock when the handle closes or the
/// process dies, so there is no age or takeover logic to get wrong.
struct ReclaimGuard {
    _file: fs::File,
}

impl ReclaimGuard {
    fn take(path: &Path) -> io::Result<Option<Self>> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(fs::TryLockError::WouldBlock) => Ok(None),
            Err(fs::TryLockError::Error(error)) => Err(error),
        }
    }
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub fn publish(dir: &Path, registration: &Registration) -> io::Result<()> {
    let json = serde_json::to_vec(registration).map_err(io::Error::other)?;
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(dir.join("office.json"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // `mode` above only applies at creation; an older registration with
        // wider permissions must not keep them, since it carries the token.
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(&json)
}

fn create_office_dir(dir: &Path) -> io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        // The mode applies only to directories this call creates, so an
        // existing office directory is never chmodded.
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)
    }
}

/// `create_new` is the whole race: exactly one caller creates the lock.
fn take_lock(lock: &Path) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(lock)?;
    write!(file, "{}", std::process::id())
}

fn is_stale(dir: &Path, lock: &Path) -> bool {
    let pid = fs::read_to_string(lock)
        .ok()
        .and_then(|raw| raw.trim().parse::<u32>().ok());
    let Some(pid) = pid else {
        // An unreadable lock only goes stale with age.
        return older_than(lock, STALE_AFTER);
    };
    match is_alive(pid) {
        Some(false) => true,
        // The winner died mid-startup if there is still no registration.
        Some(true) => !dir.join("office.json").exists() && older_than(lock, STALE_AFTER),
        // Could not ask `kill`; let age decide rather than steal a live one.
        None => older_than(lock, STALE_AFTER),
    }
}

/// `kill(pid, 0)` without libc. `None` means the question could not be asked.
fn is_alive(pid: u32) -> Option<bool> {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()
        .map(|status| status.success())
}

fn older_than(path: &Path, age: Duration) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|elapsed| elapsed > age)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    #[test]
    fn only_one_caller_wins_the_claim() {
        let dir = tempdir();
        assert!(claim(dir.path()).expect("first"), "the first caller wins");
        assert!(
            !claim(dir.path()).expect("second"),
            "the second must join instead"
        );
    }

    #[test]
    fn a_registration_round_trips() {
        let dir = tempdir();
        let registration = Registration {
            port: 7777,
            pid: std::process::id(),
            token: "t".into(),
        };
        publish(dir.path(), &registration).expect("publish");
        assert_eq!(find(dir.path()).expect("found").port, 7777);
    }

    #[test]
    fn a_missing_directory_is_not_an_error() {
        assert!(find(Path::new("/nonexistent/rigagent-office")).is_none());
    }

    #[test]
    fn a_lock_whose_winner_died_before_registering_is_reclaimed() {
        let dir = tempdir();
        let mut child = Command::new("true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn true");
        let dead = child.id();
        child.wait().expect("reap the child");
        std::fs::write(dir.path().join("office.lock"), dead.to_string()).expect("a dead pid");
        assert!(
            claim(dir.path()).expect("reclaim"),
            "a lock with no registration and a dead owner must not hold the office"
        );
        assert!(
            !claim(dir.path()).expect("second"),
            "the reclaimer now holds the office"
        );
    }

    #[test]
    fn concurrent_reclaimers_crown_exactly_one_winner() {
        use std::sync::{Arc, Barrier};

        for iteration in 0..25 {
            let dir = tempdir();
            let mut child = Command::new("true")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn true");
            let dead = child.id();
            child.wait().expect("reap the child");
            std::fs::write(dir.path().join("office.lock"), dead.to_string()).expect("a dead pid");

            let barrier = Arc::new(Barrier::new(2));
            let won = std::thread::scope(|scope| {
                let handles: Vec<_> = (0..2)
                    .map(|_| {
                        let barrier = Arc::clone(&barrier);
                        let dir = dir.path();
                        scope.spawn(move || {
                            barrier.wait();
                            claim(dir).expect("claim")
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|handle| handle.join().expect("claim thread"))
                    .filter(|won| *won)
                    .count()
            });
            assert_eq!(won, 1, "iteration {iteration}: exactly one reclaimer wins");
        }
    }

    #[test]
    fn a_held_reclaim_guard_makes_a_reclaimer_defer() {
        let dir = tempdir();
        let mut child = Command::new("true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn true");
        let dead = child.id();
        child.wait().expect("reap the child");
        std::fs::write(dir.path().join("office.lock"), dead.to_string()).expect("a dead pid");

        let held = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.path().join("office.reclaim"))
            .expect("guard file");
        held.try_lock().expect("hold the guard");

        assert!(
            !claim(dir.path()).expect("defer"),
            "a reclaimer defers while the guard is held"
        );

        drop(held);
        assert!(
            claim(dir.path()).expect("reclaim"),
            "the guard released, so the reclaimer wins"
        );
    }

    #[test]
    fn a_claim_creates_a_missing_directory() {
        let root = tempdir();
        let dir = root.path().join("not-yet");
        assert!(claim(&dir).expect("claim"), "the first caller wins");
        assert!(dir.is_dir(), "claim creates the office directory");
    }

    #[test]
    fn a_file_where_the_office_should_be_is_an_error() {
        let root = tempdir();
        let path = root.path().join("office");
        std::fs::write(&path, "not a directory").expect("a file in the way");
        assert!(claim(&path).is_err(), "a non-directory path is an error");
    }

    #[cfg(unix)]
    #[test]
    fn publishing_tightens_an_existing_registration() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir();
        let path = dir.path().join("office.json");
        std::fs::write(&path, "an old registration").expect("old registration");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("a wide old mode");

        let registration = Registration {
            port: 7777,
            pid: std::process::id(),
            token: "t".into(),
        };
        publish(dir.path(), &registration).expect("publish");

        let mode = std::fs::metadata(&path)
            .expect("registration")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "an existing wider mode is tightened");
    }

    #[cfg(unix)]
    #[test]
    fn the_office_dir_and_registration_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempdir();
        let dir = root.path().join("office");
        assert!(claim(&dir).expect("claim"));
        let registration = Registration {
            port: 7777,
            pid: std::process::id(),
            token: "t".into(),
        };
        publish(&dir, &registration).expect("publish");

        let dir_mode = std::fs::metadata(&dir).expect("dir").permissions().mode();
        assert_eq!(dir_mode & 0o777, 0o700, "office dir is private");
        let file_mode = std::fs::metadata(dir.join("office.json"))
            .expect("registration")
            .permissions()
            .mode();
        assert_eq!(
            file_mode & 0o777,
            0o600,
            "the token stays readable by us only"
        );
    }
}
