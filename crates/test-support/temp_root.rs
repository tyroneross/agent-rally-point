// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! The ONE way a test gets scratch space on disk.
//!
//! # Why this file exists
//!
//! Tests asked `std::env::temp_dir()` for a folder at 167 places in 85 files,
//! through 89 separately written helpers. 35 files deleted what they made; 50
//! did not. A test that failed early skipped its trailing `remove_dir_all`, and
//! nothing at all survived a killed process. Measured 2026-11-09: one full run
//! of the suite left about 290 new entries in the system temp directory, and
//! 1,183 older ones were removed by hand.
//!
//! # What it guarantees
//!
//! * A [`TempRoot`] deletes its folder when it drops, whether the test passed
//!   or panicked.
//! * A process that is killed leaves its folders behind, and the NEXT test
//!   process removes them on its first use of this file.
//! * Paths stay short. Everything lives under `<temp>/rt/<pid>-<n>/`, which is
//!   shorter than the names it replaces. Redirecting scratch space into the
//!   build folder was tried and rejected: the longer prefix pushed Unix socket
//!   paths past the platform limit (47 panics, 35 of them `SUN_LEN`).
//!
//! # Ownership is a lock, not a process id
//!
//! Each process holds an exclusive lock on `<temp>/rt/<pid>-<n>.lock` for as
//! long as it lives. The kernel drops the lock when the process ends, however
//! it ends. A sweeper that can take the lock therefore knows the owner is
//! gone. A process id cannot say that: ids are reused, and two sweepers could
//! both act on one stale id.
//!
//! # Rules for fixtures
//!
//! * Stop child processes before the root drops. Keep your own `Drop` and hold
//!   the [`TempRoot`] as the LAST field, so it drops after your teardown.
//! * A fixture created once per process uses [`TempRoot::process_scoped`]. It
//!   is not removed at exit; the next test process sweeps it.
//! * A path that must not exist is a child of a root that you never create.
//! * Code that only needs to READ the system temp root keeps calling
//!   `std::env::temp_dir()` and carries a `// temp-root-read: <reason>` note.
//!
//! This file is pulled into each crate's tests with `#[path]`. It uses only the
//! standard library, so no crate gains a dependency.

#![allow(dead_code)]

use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, Once};

/// Directory under the system temp root that holds every test process's area.
const AREA_DIR: &str = "rt";
/// Suffix of the per-process ownership lock, a sibling of the process area.
const LOCK_SUFFIX: &str = ".lock";
/// Longest label a caller may pass. Bounded so a fixture cannot spend the
/// socket-path budget on its own name.
const MAX_LABEL_LEN: usize = 48;

/// One process's area: a folder plus the lock that proves the owner is alive.
struct ProcessArea {
    dir: PathBuf,
    lock_path: PathBuf,
    /// Held for its lock. Dropping it releases ownership.
    _lock: File,
    /// Live roots. The area is removed when this returns to zero.
    live: usize,
}

static AREA: Mutex<Option<ProcessArea>> = Mutex::new(None);
static SWEEP: Once = Once::new();
static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);
/// Area names are never reused inside one process. A sweeper in another
/// process may still hold a handle to an area this process already removed;
/// reusing the name would let that sweeper delete the replacement.
static NEXT_AREA: AtomicU64 = AtomicU64::new(0);

fn base_dir() -> PathBuf {
    std::env::temp_dir().join(AREA_DIR)
}

fn lock_area() -> std::sync::MutexGuard<'static, Option<ProcessArea>> {
    // A test that panics while holding this guard must not take every later
    // test in the binary down with a poison error.
    AREA.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Remove the areas of processes that no longer exist.
///
/// A lock we can take means its owner is gone. A lock we cannot take, for any
/// reason, means leave it alone. The folder goes first and the lock file last,
/// so an interrupted sweep leaves a state the next sweep finishes.
fn sweep_abandoned(base: &Path) {
    let Ok(entries) = fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let lock_path = entry.path();
        let Some(name) = lock_path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(area_name) = name.strip_suffix(LOCK_SUFFIX) else {
            continue;
        };
        let Ok(lock) = OpenOptions::new().read(true).write(true).open(&lock_path) else {
            continue;
        };
        if lock.try_lock().is_err() {
            continue;
        }
        // The owner may have removed its own area between our `open` and our
        // `try_lock`. Then we hold a lock on a file that is no longer at this
        // path, and the path may already belong to someone else.
        if !same_file(&lock, &lock_path) {
            continue;
        }
        let _ = fs::remove_dir_all(base.join(area_name));
        let _ = fs::remove_file(&lock_path);
    }
}

/// Whether `open` is still the file found at `path`.
#[cfg(unix)]
fn same_file(open: &File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (open.metadata(), fs::metadata(path)) {
        (Ok(held), Ok(on_disk)) => held.dev() == on_disk.dev() && held.ino() == on_disk.ino(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_file(_open: &File, path: &Path) -> bool {
    path.exists()
}

/// Create this process's area. The lock file is created exclusively and locked
/// BEFORE the folder exists, so a concurrent sweeper sees either a held lock or
/// nothing it can mistake for an abandoned area. If a sweeper wins the lock in
/// the gap between creation and locking, this attempt is abandoned and the
/// next name is tried.
fn create_area(base: &Path) -> ProcessArea {
    let pid = std::process::id();
    loop {
        let attempt = NEXT_AREA.fetch_add(1, Ordering::Relaxed);
        let name = format!("{pid}-{attempt}");
        let lock_path = base.join(format!("{name}{LOCK_SUFFIX}"));
        let lock = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(file) => file,
            Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
            Err(err) => panic!(
                "test temp root: cannot create {}: {err}",
                lock_path.display()
            ),
        };
        if lock.try_lock().is_err() {
            continue;
        }
        let dir = base.join(&name);
        if let Err(err) = fs::create_dir(&dir) {
            panic!("test temp root: cannot create {}: {err}", dir.display());
        }
        return ProcessArea {
            dir,
            lock_path,
            _lock: lock,
            live: 0,
        };
    }
}

fn validate_label(label: &str) {
    let ok = !label.is_empty()
        && label.len() <= MAX_LABEL_LEN
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        && !label.starts_with('.');
    assert!(
        ok,
        "test temp root: label {label:?} must be 1 to {MAX_LABEL_LEN} characters of \
         ASCII letters, digits, '-', '_' or '.', and must not start with '.'"
    );
}

/// Create `<area>/<label>-<n>` and count it as live.
fn allocate(label: &str) -> PathBuf {
    validate_label(label);
    let base = base_dir();
    fs::create_dir_all(&base)
        .unwrap_or_else(|err| panic!("test temp root: cannot create {}: {err}", base.display()));
    SWEEP.call_once(|| sweep_abandoned(&base));

    let mut area = lock_area();
    let area = area.get_or_insert_with(|| create_area(&base));
    let n = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
    let path = area.dir.join(format!("{label}-{n}"));
    fs::create_dir(&path)
        .unwrap_or_else(|err| panic!("test temp root: cannot create {}: {err}", path.display()));
    area.live += 1;
    path
}

/// A scratch folder that is deleted when this value drops.
pub struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    /// Create a fresh, empty folder. `label` appears in the folder name and is
    /// followed by a per-process counter, so two roots never collide.
    pub fn new(label: &str) -> Self {
        Self {
            path: allocate(label),
        }
    }

    /// A folder that lives until the process exits, for fixtures built once
    /// per test binary. The next test process removes it.
    pub fn process_scoped(label: &str) -> &'static Path {
        Box::leak(allocate(label).into_boxed_path())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Deref for TempRoot {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TempRoot {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

/// Lets `&root` stand in wherever a call site passed `&PathBuf` to an
/// `impl Into<PathBuf>` parameter, so migrating a helper does not have to
/// rewrite every caller.
impl From<&TempRoot> for PathBuf {
    fn from(root: &TempRoot) -> PathBuf {
        root.path.clone()
    }
}

impl std::fmt::Debug for TempRoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("TempRoot").field(&self.path).finish()
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
        let mut area = lock_area();
        let Some(current) = area.as_mut() else {
            return;
        };
        current.live = current.live.saturating_sub(1);
        if current.live == 0 {
            // Nothing in this process still needs the area. Remove it now so
            // an ordinary run leaves nothing for a later sweep. The folder
            // goes first and the lock file last, matching `sweep_abandoned`.
            if let Some(done) = area.take() {
                let _ = fs::remove_dir_all(&done.dir);
                let _ = fs::remove_file(&done.lock_path);
            }
        }
    }
}
