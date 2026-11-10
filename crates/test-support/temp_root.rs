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
//! * A fixture whose path becomes a Unix socket address uses
//!   [`TempRoot::short`], which lives under `/tmp`. Never build a `/tmp` path
//!   by hand: it is not swept and not removed when a test is killed.
//! * A label is a hint for humans. It is sanitised and cut to 48 characters,
//!   so it cannot fail a test or escape the process area.
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
/// Longest label kept in a folder name. Bounded so a fixture cannot spend the
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

/// One place areas live, with this process's area in it. There are two:
/// the system temp directory, and a short one for fixtures whose paths must
/// fit a Unix socket address.
struct Space {
    short: bool,
    area: Mutex<Option<ProcessArea>>,
    sweep: Once,
}

impl Space {
    const fn new(short: bool) -> Self {
        Self {
            short,
            area: Mutex::new(None),
            sweep: Once::new(),
        }
    }

    fn base_dir(&self) -> PathBuf {
        if self.short {
            short_base_dir()
        } else {
            std::env::temp_dir().join(AREA_DIR)
        }
    }

    fn lock_area(&self) -> std::sync::MutexGuard<'_, Option<ProcessArea>> {
        // A test that panics while holding this guard must not take every
        // later test in the binary down with a poison error.
        self.area
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

static STANDARD: Space = Space::new(false);
static SHORT: Space = Space::new(true);
static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);
/// Area names are never reused inside one process. A sweeper in another
/// process may still hold a handle to an area this process already removed;
/// reusing the name would let that sweeper delete the replacement.
static NEXT_AREA: AtomicU64 = AtomicU64::new(0);

/// Base for [`TempRoot::short`]. On macOS the system temp directory is about
/// 49 characters (57 once `/var` is resolved to `/private/var`), which leaves
/// too little of a 104-byte socket address for `HOME/.local/share/...`. `/tmp`
/// is the shortest directory every Unix has.
#[cfg(unix)]
fn short_base_dir() -> PathBuf {
    PathBuf::from("/tmp").join(AREA_DIR)
}

#[cfg(not(unix))]
fn short_base_dir() -> PathBuf {
    std::env::temp_dir().join(AREA_DIR)
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

/// Turn any caller-supplied label into a safe, bounded folder-name stem.
///
/// A label only makes a leftover folder recognisable; the per-process counter
/// is what makes a name unique. So a label is never a reason to fail a test:
/// anything outside ASCII letters, digits, `-`, `_` and `.` becomes `-`, a
/// leading `.` is dropped, the result is cut to [`MAX_LABEL_LEN`], and an empty
/// result becomes `root`. No input can escape the process area or spend the
/// socket-path budget. (An earlier version panicked on a long label. The first
/// lane merge tripped it with a 49-character label assembled by a helper and
/// its caller, which neither could see alone.)
fn safe_label(label: &str) -> String {
    let mut out: String = label
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') {
                b as char
            } else {
                '-'
            }
        })
        .collect();
    let leading_dots = out.len() - out.trim_start_matches('.').len();
    out.drain(..leading_dots);
    out.truncate(MAX_LABEL_LEN);
    if out.is_empty() {
        out.push_str("root");
    }
    out
}

/// Create `<area>/<label>-<n>` and count it as live.
fn allocate(space: &'static Space, label: &str) -> PathBuf {
    let label = safe_label(label);
    let base = space.base_dir();
    fs::create_dir_all(&base)
        .unwrap_or_else(|err| panic!("test temp root: cannot create {}: {err}", base.display()));
    space.sweep.call_once(|| sweep_abandoned(&base));

    let mut area = space.lock_area();
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
    space: &'static Space,
}

impl TempRoot {
    /// Create a fresh, empty folder. `label` appears in the folder name and is
    /// followed by a per-process counter, so two roots never collide.
    pub fn new(label: &str) -> Self {
        Self {
            path: allocate(&STANDARD, label),
            space: &STANDARD,
        }
    }

    /// Like [`TempRoot::new`], but under `/tmp` so the path is short. Use it
    /// only when a path derived from this folder becomes a Unix socket
    /// address; everything else belongs in the system temp directory. Same
    /// cleanup, same ownership lock, same sweep.
    pub fn short(label: &str) -> Self {
        Self {
            path: allocate(&SHORT, label),
            space: &SHORT,
        }
    }

    /// A folder that lives until the process exits, for fixtures built once
    /// per test binary. The next test process removes it.
    pub fn process_scoped(label: &str) -> &'static Path {
        Box::leak(allocate(&STANDARD, label).into_boxed_path())
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
        let mut area = self.space.lock_area();
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
