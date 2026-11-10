// SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Stress test and timing probes for `crates/test-support/temp_root.rs`.
//!
//! The stress test always runs, at a small size: many processes hold and churn
//! scratch folders while others are killed and others sweep, and no live
//! process may ever lose a folder it holds. That is the property every test in
//! the workspace relies on, and a unit test of one sweep cannot show it.
//!
//! The timing probes print numbers and assert nothing, so they run only when
//! asked:
//!   RALLY_TEMP_ROOT_PROBE=1 cargo test --release -p rally-protocol \
//!     --test temp_root_probe -- --nocapture --test-threads=1
//!
//! Measured 2026-11-09 (release build, 16-core Mac):
//! * a root costs 162 microseconds to create and drop, against 139 for a bare
//!   `create_dir` + `remove_dir_all`; 452 when it is the only live root, since
//!   the process area is then removed and recreated each time;
//! * first use in a process costs 228 microseconds with nothing left on disk,
//!   369 with 100 kept areas and 1,572 with 1,000 (5,834 and 56,660 before the
//!   sweep read kept areas from one directory listing);
//! * removing abandoned areas is paid once, by the first process to see them:
//!   about 0.42 milliseconds each.

#[path = "../../test-support/temp_root.rs"]
mod temp_root;

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use temp_root::TempRoot;

const ROLE: &str = "RALLY_TEMP_ROOT_PROBE_CHILD";

fn enabled() -> bool {
    std::env::var_os("RALLY_TEMP_ROOT_PROBE").is_some()
}

#[test]
fn child_entry() {
    let Ok(role) = std::env::var(ROLE) else {
        return;
    };
    match role.as_str() {
        "first" => {
            let started = Instant::now();
            let root = TempRoot::new("first");
            println!("FIRST_US={}", started.elapsed().as_micros());
            drop(root);
        }
        "first-short" => {
            let started = Instant::now();
            let root = TempRoot::short("first");
            println!("FIRST_US={}", started.elapsed().as_micros());
            drop(root);
        }
        "churn" => {
            let ms: u64 = std::env::var("CHURN_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(2000);
            let end = Instant::now() + Duration::from_millis(ms);
            let mut n = 0u64;
            // One long-lived root, checked every round: a sweeper must never take it.
            let anchor = TempRoot::new("anchor");
            std::fs::write(anchor.join("a"), b"x").unwrap();
            println!("READY");
            while Instant::now() < end {
                let root = if n.is_multiple_of(2) {
                    TempRoot::new("churn")
                } else {
                    TempRoot::short("churn")
                };
                let file = root.join("f");
                std::fs::write(&file, b"x").unwrap();
                std::thread::sleep(Duration::from_millis(1 + n % 3));
                if !file.is_file() || !anchor.join("a").is_file() {
                    println!("LOST after {n} rounds: {}", file.display());
                    std::process::exit(42);
                }
                drop(root);
                n += 1;
            }
            println!("ROUNDS={n}");
        }
        other => panic!("unknown role {other}"),
    }
}

fn spawn(role: &str, envs: &[(&str, &str)]) -> Child {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", "child_entry", "--nocapture", "--test-threads=1"])
        .env(ROLE, role)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.spawn().unwrap()
}

fn first_use_us(role: &str) -> u128 {
    let mut child = spawn(role, &[]);
    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    assert!(child.wait().unwrap().success());
    let line = out
        .lines()
        .find(|l| l.contains("FIRST_US="))
        .expect("first-use line");
    line[line.find("FIRST_US=").unwrap() + 9..]
        .trim()
        .parse()
        .unwrap()
}

fn median(mut v: Vec<u128>) -> u128 {
    v.sort();
    v[v.len() / 2]
}

#[test]
fn probe_1_per_call_cost() {
    if !enabled() {
        return;
    }
    const N: u32 = 2000;
    let warm = TempRoot::new("warm");
    let started = Instant::now();
    for _ in 0..N {
        let root = TempRoot::new("bench");
        drop(root);
    }
    let helper = started.elapsed();
    let base = std::env::temp_dir().join(format!("probe-baseline-{}", std::process::id())); // temp-root-read: baseline for the probe
    std::fs::create_dir_all(&base).unwrap();
    let started = Instant::now();
    for i in 0..N {
        let dir = base.join(format!("d-{i}"));
        std::fs::create_dir(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
    let raw = started.elapsed();
    std::fs::remove_dir_all(&base).unwrap();
    drop(warm);
    println!(
        "PROBE per_call helper_us={} raw_mkdir_rmdir_us={} (mean of {N})",
        helper.as_micros() / N as u128,
        raw.as_micros() / N as u128
    );

    // Worst case for allocation: every root is the only live one, so each
    // drop removes the area and each new recreates it.
    let started = Instant::now();
    for _ in 0..N {
        let root = TempRoot::new("solo");
        drop(root);
    }
    println!(
        "PROBE per_call_recreating_area_us={} (mean of {N})",
        started.elapsed().as_micros() / N as u128
    );
}

fn make_kept(base: &std::path::Path, count: usize, tag: &str) -> Vec<PathBuf> {
    let mut made = Vec::new();
    for i in 0..count {
        let name = format!("999{tag}{i}-0");
        let dir = base.join(&name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(base.join(format!("{name}.lock")), b"").unwrap();
        std::fs::write(base.join(format!("{name}.keep")), b"").unwrap();
        made.push(dir);
    }
    made
}

fn remove_kept(base: &std::path::Path, made: Vec<PathBuf>) {
    for dir in made {
        let name = dir.file_name().unwrap().to_str().unwrap().to_string();
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(base.join(format!("{name}.lock")));
        let _ = std::fs::remove_file(base.join(format!("{name}.keep")));
    }
}

#[test]
fn probe_2_first_use_cost_by_leftovers() {
    if !enabled() {
        return;
    }
    let base = std::env::temp_dir().join("rt"); // temp-root-read: probe plants leftovers beside real areas
    std::fs::create_dir_all(&base).unwrap();
    for kept in [0usize, 100, 1000] {
        let made = make_kept(&base, kept, "k");
        let samples: Vec<u128> = (0..15).map(|_| first_use_us("first")).collect();
        println!(
            "PROBE first_use kept_areas={kept} median_us={} max_us={}",
            median(samples.clone()),
            samples.iter().max().unwrap()
        );
        remove_kept(&base, made);
    }
    // Abandoned areas are removed by the first process that sees them: a one-time cost.
    for abandoned in [100usize, 1000] {
        for i in 0..abandoned {
            let name = format!("998a{i}-0");
            let dir = base.join(&name);
            std::fs::create_dir_all(dir.join("x-0")).unwrap();
            std::fs::write(dir.join("x-0/file"), b"x").unwrap();
            std::fs::write(base.join(format!("{name}.lock")), b"").unwrap();
        }
        let first = first_use_us("first");
        let second = first_use_us("first");
        println!(
            "PROBE first_use abandoned_areas={abandoned} first_process_us={first} next_process_us={second}"
        );
    }
}

#[test]
fn probe_3_stress_live_roots_are_never_taken() {
    // Small by default so it belongs in every run; the larger size is the one
    // the audit used.
    let (churners, churn_ms, run_ms) = if enabled() {
        (48usize, "3000", 2500u64)
    } else {
        (12usize, "1200", 900u64)
    };
    let mut children: Vec<(Child, BufReader<std::process::ChildStdout>)> = Vec::new();
    for _ in 0..churners {
        let mut c = spawn("churn", &[("CHURN_MS", churn_ms)]);
        let out = BufReader::new(c.stdout.take().unwrap());
        children.push((c, out));
    }
    // Wait until every churner holds its anchor root.
    for (_, out) in children.iter_mut() {
        let mut line = String::new();
        loop {
            line.clear();
            assert!(out.read_line(&mut line).unwrap() > 0, "churner ended early");
            if line.contains("READY") {
                break;
            }
        }
    }
    // Kill a third at staggered times while sweepers start continuously.
    let started = Instant::now();
    let mut sweeps = 0u32;
    let mut killed = 0usize;
    while started.elapsed() < Duration::from_millis(run_ms) {
        let _ = first_use_us("first");
        let _ = first_use_us("first-short");
        sweeps += 2;
        if killed < churners / 3 {
            children[killed * 3].0.kill().unwrap();
            killed += 1;
        }
    }
    let mut lost = 0;
    let mut finished = 0;
    let mut rounds = 0u64;
    for (i, (mut child, mut out)) in children.into_iter().enumerate() {
        let mut rest = String::new();
        let _ = out.read_to_string(&mut rest);
        let status = child.wait().unwrap();
        if i.is_multiple_of(3) && i / 3 < killed {
            continue;
        }
        if status.code() == Some(42) || rest.contains("LOST") {
            lost += 1;
            println!("{rest}");
        }
        if status.success() {
            finished += 1;
        }
        if let Some(l) = rest.lines().find(|l| l.contains("ROUNDS=")) {
            rounds += l[l.find("ROUNDS=").unwrap() + 7..]
                .trim()
                .parse::<u64>()
                .unwrap();
        }
    }
    let _ = first_use_us("first");
    let _ = first_use_us("first-short");
    let left = |p: PathBuf| std::fs::read_dir(p).map(|d| d.count()).unwrap_or(0);
    println!(
        "PROBE stress churners={churners} killed={killed} sweeps={sweeps} finished_ok={finished} lost={lost} rounds={rounds} left_std={} left_short={}",
        left(std::env::temp_dir().join("rt")), // temp-root-read: probe inspects the helper's area
        left(PathBuf::from("/tmp/rt"))
    );
    assert_eq!(lost, 0, "a live process lost a folder it was holding");
    assert_eq!(
        finished,
        churners - killed,
        "every surviving churner must finish cleanly"
    );
}
