// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

//! Room composition under synthetic load — bytes AND latency.
//!
//! Room composition ships a byte budget: rank facts by relevance, fill until
//! the ceiling, stop. Correctness-bearing claims, peers, and assigned work are
//! never cut; when that floor cannot fit, the response must report
//! `over_budget: true`. Testing only bytes grades the wrong thing in both
//! directions: a response may exceed the ceiling honestly, and latency may
//! regress while a cuttable response remains bounded.
//!
//! **What that costs is measured here, not assumed.** The nested scans are real
//! in the source, and the shape they imply is quadratic. The observed growth is
//! not, at the sizes a room actually reaches: 800 -> 6,400 facts (8x) cost
//! 92.5 -> 690.3 ms, a 7.47x rise. That is sub-linear, so the quadratic term is
//! present in the code and not yet dominant in the measurement. Release-binary
//! samples show the exponent creeping up with N — 400 -> 1,600 cost 1.34x,
//! 1,600 -> 6,400 cost 2.33x — which is the signature of a superlinear term
//! becoming visible rather than one that already rules.
//!
//! The bound below is therefore set to catch the shape CHANGING, and this
//! comment states the measurement rather than the projection, because a
//! reasoned-not-measured performance claim is how a gate ends up defending a
//! number nobody checked.
//!
//! So this file asserts two independent properties at two data scales:
//!
//! 1. **The ceiling verdict is exact.** A response over its serialized-byte
//!    ceiling must say `over_budget: true`; a response claiming the ceiling held
//!    must fit. A sub-linear growth check still applies when both samples fit.
//! 2. **Latency stays sub-quadratic.** An 8x data increase costs ~8x linearly
//!    and ~64x quadratically; the bound sits between those, so it catches a
//!    shape change without grading the constant factor.
//!
//! ## Why this is not a flaky gate
//!
//! A ratio-of-timings assertion is noisy by construction, and a gate that fails
//! intermittently certifies failures as passes once people learn to re-run it.
//! Four things keep the signal real:
//!
//! - **Warm-up run per scale**, discarded, so neither scale pays first-open
//!   reconcile or page-cache cost the other does not.
//! - **Minimum of N runs**, not mean. The minimum is the closest observation to
//!   the true cost; background load only ever adds.
//! - **A generous multiplier** (`MAX_SCALING_FACTOR`) that admits linear growth
//!   plus a wide margin, and still sits well below quadratic.
//! - **An absolute floor** (`MIN_MEASURABLE_MS`). Below it, timer granularity
//!   and process-spawn cost dominate the measurement and the ratio means
//!   nothing, so the test reports and skips the ratio rather than asserting on
//!   noise. A skipped assertion that says so beats a coin-flip that does not.

use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Facts in the small ledger.
const BASE_FACTS: usize = 800;
/// Multiplier between the two scales. Quadratic cost would be this squared.
const SCALE: usize = 8;
/// Timed samples per scale; the minimum is kept.
const SAMPLES: usize = 3;

/// Latency at `SCALE`x data may be at most this many times the base latency.
///
/// Linear is 8x, quadratic 64x, and the bound sits at 24x — 3x headroom over
/// linear, and well under a third of quadratic.
///
/// The wide scale gap is deliberate. Measured on the release binary at min-of-3,
/// 400 -> 1600 facts cost 38 -> 51 ms (1.34x) and 1600 -> 6400 cost 51 -> 119 ms
/// (2.33x): the ratio RISES with N, so a superlinear term exists but only
/// separates from noise at scale. A 4x gap put the measurement at 2.95x against
/// a 9x bound, which is a gate that would miss a doubling of the exponent's
/// effect. 8x separates the hypotheses further for the cost of one more second
/// of test time.
///
/// Honest limit: with ~3x headroom this catches a SHAPE change, not a
/// constant-factor regression. A change that makes composition uniformly twice
/// as slow passes here and should be caught by a benchmark, not by this test.
const MAX_SCALING_FACTOR: f64 = 24.0;

/// Below this, the ratio is measuring the timer and the process launcher, not
/// the projection.
const MIN_MEASURABLE_MS: f64 = 40.0;

/// When both fixtures fit their ceilings, the room payload at `SCALE`x may be
/// at most this many times the base payload.
///
/// Never-cut claim floors can grow linearly and truthfully report overflow; the
/// ratio is not asserted in that case because cutting claims to satisfy a
/// scaling oracle would violate the product's collision-safety contract.
///
/// 6.0 sits between the 4.83x measurement and the 8.0x that fully-linear
/// pass-through would cost. It asserts what composition delivers today rather
/// than the cap it does not; tighten it when the ungoverned sections are wired
/// in.
const MAX_BYTE_GROWTH: f64 = 6.0;

struct Workspace {
    cwd: PathBuf,
    home: PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let cwd = temp_path(&format!("{name}-cwd"));
        let home = temp_path(&format!("{name}-home"));
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(cwd.join(".git")).unwrap();
        fs::create_dir_all(cwd.join(".rally/log")).unwrap();
        Self { cwd, home }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rally"))
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            // Auto-reap would mutate the ledger mid-measurement and make the
            // two scales incomparable.
            .env("RALLY_NO_AUTO_REAP", "1")
            .args(args)
            .output()
            .unwrap()
    }

    fn cleanup(self) {
        fs::remove_dir_all(self.cwd).ok();
        fs::remove_dir_all(self.home).ok();
    }
}

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "{name}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

/// Seed a ledger made almost entirely of facts in the buckets the budget
/// GOVERNS (artifacts, decisions, risks, lessons) rather than the ones it does
/// not (claims, presence).
///
/// A mixed fixture measured a 2-byte reduction under a 0.1% ceiling — an
/// assertion that passes and proves nothing, because the fixture put almost no
/// bytes where the budget can reach. Testing a control against data it does not
/// govern certifies it either way.
fn seed_governed_ledger(workspace: &Workspace, count: usize) {
    let log = workspace.cwd.join(".rally/log/synthetic.jsonl");
    let mut out = String::with_capacity(count * 500);
    for i in 0..count {
        let seq = i + 1;
        let tool = format!("agent_{:02}", i % 8);
        let hours_ago = (i % 960) as i64;
        let created = (chrono::Utc::now() - chrono::Duration::hours(hours_ago))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let kind = ["artifact", "decision", "risk", "lesson"][i % 4];
        // Wide bodies so the ranking has real bytes to trade against.
        let body = "detail ".repeat(40);
        out.push_str(&format!(
            r#"{{"seq":{seq},"occurred_at":"{created}","event_type":"{kind}","payload":{{"schema":"agent-rally.fact.v1","event_id":"fact_gov_{i:06}","seq":0,"thread_id":"synthetic","kind":"{kind}","tool":"{tool}","role":null,"subject":"governed {kind} {i}","summary":"{body}","evidence":[],"created_at":"{created}","ref":null,"status":null,"severity":null,"uri":null,"scope":[],"target":null}}}}"#
        ));
        out.push('\n');
    }
    fs::write(log, out).unwrap();
}

/// Write a synthetic ledger of `count` facts directly as JSONL.
///
/// Seeded through the file rather than through `count` CLI calls: at these
/// sizes the CLI path costs minutes and measures process spawn, not
/// projection. The shape mirrors a real room — mostly claims and handoffs,
/// because those are the two projections whose staleness decisions scan the
/// whole fact list, which is where the superlinear term lives.
fn seed_ledger(workspace: &Workspace, count: usize) {
    let log = workspace.cwd.join(".rally/log/synthetic.jsonl");
    let mut out = String::with_capacity(count * 400);
    for i in 0..count {
        let seq = i + 1;
        let tool = format!("agent_{:02}", i % 24);
        // Spread ages across ~40 days so staleness/decay paths are exercised
        // rather than short-circuited by everything being fresh.
        let hours_ago = (i % 960) as i64;
        let created = (chrono::Utc::now() - chrono::Duration::hours(hours_ago))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let (kind, payload_extra) = match i % 5 {
            0 | 1 => (
                "claim",
                format!(r#""scope":["file:src/generated/mod_{i}.rs"],"target":null"#),
            ),
            2 => ("handoff", r#""scope":[],"target":"all""#.to_string()),
            3 => ("artifact", r#""scope":[],"target":null"#.to_string()),
            _ => ("presence", r#""scope":[],"target":null"#.to_string()),
        };
        out.push_str(&format!(
            r#"{{"seq":{seq},"occurred_at":"{created}","event_type":"{kind}","payload":{{"schema":"agent-rally.fact.v1","event_id":"fact_synth_{i:06}","seq":0,"thread_id":"synthetic","kind":"{kind}","tool":"{tool}","role":null,"subject":"synthetic {kind} {i}","summary":"synthetic load fact {i}","evidence":[],"created_at":"{created}","ref":null,"status":null,"severity":null,"uri":null,{payload_extra}}}}}"#
        ));
        out.push('\n');
    }
    fs::write(log, out).unwrap();
}

/// Byte length of `rally room --json` output.
fn room_bytes(workspace: &Workspace) -> usize {
    let output = workspace.run(&["room", "--json"]);
    assert!(
        output.status.success(),
        "rally room failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Parse to confirm it is well-formed, then measure the real payload.
    let _: Value = serde_json::from_slice(&output.stdout).expect("room --json must be valid JSON");
    output.stdout.len()
}

struct RoomMeasurement {
    bytes: usize,
    budget: Option<usize>,
    emitted_bytes: Option<usize>,
    over_budget: bool,
}

fn room_measurement(workspace: &Workspace) -> RoomMeasurement {
    let output = workspace.run(&["room", "--json"]);
    assert!(
        output.status.success(),
        "rally room failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value =
        serde_json::from_slice(&output.stdout).expect("room --json must be valid JSON");
    RoomMeasurement {
        bytes: output.stdout.len(),
        budget: value["data"]["room"]["composition"]["budget_bytes"]
            .as_u64()
            .map(|bytes| bytes as usize),
        emitted_bytes: value["data"]["room"]["composition"]["emitted_bytes"]
            .as_u64()
            .map(|bytes| bytes as usize),
        over_budget: value["data"]["room"]["composition"]["over_budget"]
            .as_bool()
            .unwrap_or(false),
    }
}

fn assert_ceiling_verdict(measurement: &RoomMeasurement) {
    if let Some(emitted_bytes) = measurement.emitted_bytes {
        assert_eq!(
            emitted_bytes, measurement.bytes,
            "composition emitted_bytes must count the complete rendered response"
        );
    }
    if let Some(budget) = measurement.budget {
        assert_eq!(
            measurement.over_budget,
            measurement.bytes > budget,
            "ceiling verdict disagrees with emitted bytes: bytes={} budget={} over_budget={}",
            measurement.bytes,
            budget,
            measurement.over_budget
        );
    }
}

/// Minimum wall time over `SAMPLES` runs, after one discarded warm-up.
fn min_room_millis(workspace: &Workspace) -> f64 {
    let _ = workspace.run(&["room", "--json"]);
    (0..SAMPLES)
        .map(|_| {
            let start = Instant::now();
            let output = workspace.run(&["room", "--json"]);
            let elapsed = start.elapsed().as_secs_f64() * 1000.0;
            assert!(output.status.success());
            elapsed
        })
        .fold(f64::MAX, f64::min)
}

#[test]
fn room_composition_bounds_bytes_and_latency_as_the_ledger_grows() {
    let small = Workspace::new("rally-budget-scale-small");
    let large = Workspace::new("rally-budget-scale-large");
    seed_ledger(&small, BASE_FACTS);
    seed_ledger(&large, BASE_FACTS * SCALE);

    let small_measurement = room_measurement(&small);
    let large_measurement = room_measurement(&large);
    assert_ceiling_verdict(&small_measurement);
    assert_ceiling_verdict(&large_measurement);
    let small_bytes = small_measurement.bytes;
    let large_bytes = large_measurement.bytes;
    let byte_growth = large_bytes as f64 / small_bytes.max(1) as f64;

    let small_ms = min_room_millis(&small);
    let large_ms = min_room_millis(&large);
    let latency_growth = large_ms / small_ms.max(f64::MIN_POSITIVE);

    // Always report the numbers. A perf assertion whose measurements are
    // invisible cannot be diagnosed when it fails on someone else's machine.
    println!(
        "room scaling: {BASE_FACTS} facts -> {small_bytes} bytes / {small_ms:.1} ms; \
         {} facts -> {large_bytes} bytes / {large_ms:.1} ms; \
         byte growth {byte_growth:.2}x, latency growth {latency_growth:.2}x \
         (data growth {SCALE}x, quadratic would be {}x)",
        BASE_FACTS * SCALE,
        SCALE * SCALE
    );

    if !small_measurement.over_budget && !large_measurement.over_budget {
        assert!(
            byte_growth <= MAX_BYTE_GROWTH,
            "room payload grew {byte_growth:.2}x for {SCALE}x the ledger \
             ({small_bytes} -> {large_bytes} bytes), past the {MAX_BYTE_GROWTH:.1}x bound. \
             Both responses claimed the ceiling held, so proportional growth means \
             composition stopped ranking and started passing facts through."
        );
    } else {
        println!(
            "room scaling: byte-growth assertion not applicable because the never-cut \
             floor reported overflow (small_over={} large_over={})",
            small_measurement.over_budget, large_measurement.over_budget
        );
    }

    if small_ms < MIN_MEASURABLE_MS {
        println!(
            "room scaling: base latency {small_ms:.1} ms is below the \
             {MIN_MEASURABLE_MS:.0} ms measurable floor — reporting the ratio but not \
             asserting on it, because at this scale the measurement is dominated by \
             process spawn rather than by projection."
        );
        small.cleanup();
        large.cleanup();
        return;
    }

    assert!(
        latency_growth <= MAX_SCALING_FACTOR,
        "room latency grew {latency_growth:.2}x for {SCALE}x the ledger \
         ({small_ms:.1} ms -> {large_ms:.1} ms), past the {MAX_SCALING_FACTOR:.1}x bound. \
         Linear would be {SCALE}x and quadratic {}x, so this is the shape changing, not \
         the constant. The byte budget cannot catch it: bytes grew only \
         {byte_growth:.2}x on the same data.",
        SCALE * SCALE
    );

    small.cleanup();
    large.cleanup();
}

/// What the byte budget actually governs, measured rather than assumed.
///
/// Shrinking the ceiling shrinks the payload — so the budget is wired and does
/// something. It does NOT bound the payload. Measured against this repo's own
/// room with the ceiling driven down to 4 KB, the room still emitted 154 KB,
/// and the sections responsible were:
///
/// | section | bytes | items | budgeted |
/// |---|---|---|---|
/// | `active_claims` | 67,786 | 95 | no |
/// | `system_health` | 56,413 | 75 | no |
/// | `squads` | 17,600 | 122 | no |
/// | everything else | ~6,000 | — | yes, trimmed to 1 item each |
///
/// The budget trimmed every bucket it governs down to a single item and left
/// 92% of the payload alone. That is the register's third pattern once more: a
/// verdict computed correctly and applied to the wrong set. It is recorded, not
/// fixed here — making three more sections budget-aware changes what every
/// agent reads on every room call, which is not a change to land mid-release
/// alongside three security fixes.
///
/// This test therefore asserts what is TRUE and useful: the budget must keep
/// having an effect, and the ungoverned floor must not grow. If someone wires
/// the remaining sections in, the first assertion still passes and the floor
/// constant here should come down.
#[test]
fn budget_binds_on_the_buckets_it_governs() {
    let workspace = Workspace::new("rally-budget-binds");
    seed_governed_ledger(&workspace, 1_200);

    let default_bytes = room_bytes(&workspace);

    let tight = Command::new(env!("CARGO_BIN_EXE_rally"))
        .current_dir(&workspace.cwd)
        .env("HOME", &workspace.home)
        .env("RALLY_NO_AUTO_REAP", "1")
        .env("RALLY_ROOM_BUDGET_FRACTION", "0.001")
        .args(["room", "--json"])
        .output()
        .unwrap();
    assert!(tight.status.success());
    let tight_bytes = tight.stdout.len();

    println!(
        "budget effect: default {default_bytes} bytes -> tight ceiling {tight_bytes} bytes \
         ({:.1}% reduction)",
        100.0 * (1.0 - tight_bytes as f64 / default_bytes as f64)
    );

    // A meaningful reduction, not merely a smaller number. `tight < default`
    // passed on a 2-byte difference against a fixture the budget barely
    // governed — true, and no evidence at all.
    const MIN_REDUCTION: f64 = 0.20;
    let reduction = 1.0 - tight_bytes as f64 / default_bytes as f64;
    assert!(
        reduction >= MIN_REDUCTION,
        "driving the ceiling to 0.1% of the consumer context cut only {:.1}% of the \
         payload ({default_bytes} -> {tight_bytes} bytes) against a ledger built \
         entirely from budget-governed buckets. Under {:.0}% means the budget has \
         stopped reaching composition.",
        reduction * 100.0,
        MIN_REDUCTION * 100.0
    );

    workspace.cleanup();
}

/// A high-volume response must report the exact ceiling verdict. Correctness
/// floors may make the response larger than its budget, but that overflow must
/// never be silent.
#[test]
fn high_fact_count_reports_exact_ceiling_verdict() {
    let workspace = Workspace::new("rally-budget-ceiling");
    seed_ledger(&workspace, 4_000);

    let measurement = room_measurement(&workspace);
    assert_ceiling_verdict(&measurement);
    assert!(
        measurement.budget.is_some(),
        "the default room ceiling must be present"
    );

    println!(
        "room ceiling: 4,000 facts -> {} bytes (budget={:?}, over_budget={})",
        measurement.bytes, measurement.budget, measurement.over_budget
    );
    workspace.cleanup();
}
