// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0

use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

struct RallyWorkspace {
    cwd: PathBuf,
    home: PathBuf,
}

impl RallyWorkspace {
    fn new(name: &str) -> Self {
        let cwd = temp_path(&format!("{name}-cwd"));
        let home = temp_path(&format!("{name}-home"));
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&home).unwrap();
        Self { cwd, home }
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_rally"))
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("RALLY_CMUX_CONFIG_DIR")
            .env_remove("RALLY_HERDR_CONFIG_DIR")
            .args(args)
            .output()
            .unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
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

fn write_json(name: &str, value: &Value) -> PathBuf {
    let path = temp_path(name).with_extension("json");
    fs::write(&path, serde_json::to_string(value).unwrap()).unwrap();
    path
}

#[test]
fn agent_starts_from_preflight_and_clears_a_required_handoff() {
    let workspace = RallyWorkspace::new("journey-preflight-handoff");

    let handoff = workspace.json(&[
        "handoff",
        "--json",
        "--to",
        "codex",
        "--from-tool",
        "pi",
        "--subject",
        "review sync import",
        "--files",
        "crates/rally-core/src/sync.rs",
        "docs/SIGNED_EVENTS.md",
    ]);
    let handoff_id = handoff["event_id"].as_str().unwrap();

    let preflight = workspace.json(&[
        "preflight",
        "--json",
        "--tool",
        "codex",
        "--session-id",
        "codex-journey",
        "--start-ping",
    ]);
    assert_eq!(preflight["routing"]["action"], "join_active");
    assert_eq!(preflight["pending_acks_for_me"][0]["event_id"], handoff_id);
    assert_eq!(
        preflight["pending_acks_for_me"][0]["files"][0],
        "crates/rally-core/src/sync.rs"
    );

    let context = workspace.json(&["context", "--json", "--tool", "codex"]);
    assert_eq!(
        context["data"]["brief"]["recommended_next_action"]["action"],
        "ack_handoff"
    );
    assert_eq!(
        context["data"]["brief"]["recommended_next_action"]["target"],
        handoff_id
    );

    workspace.json(&[
        "ack",
        "--json",
        "--tool",
        "codex",
        "--summary",
        "review complete",
        handoff_id,
    ]);
    let inbox = workspace.json(&["inbox", "--json", "--tool", "codex"]);
    workspace.cleanup();

    assert_eq!(inbox["data"]["pending"].as_array().unwrap().len(), 0);
}

#[test]
fn agents_detect_and_resolve_overlapping_file_claims() {
    let workspace = RallyWorkspace::new("journey-file-claims");

    let codex_claim = workspace.json(&[
        "claim",
        "--json",
        "--tool",
        "codex",
        "--path",
        "crates/rally-core/src/query.rs",
        "--subject",
        "tighten projections",
    ]);
    let codex_claim_id = codex_claim["event_id"].as_str().unwrap();
    workspace.json(&[
        "claim",
        "--json",
        "--tool",
        "pi",
        "--path",
        "crates/rally-core/src/query.rs",
        "--subject",
        "review projections",
    ]);

    let conflicts = workspace.json(&["conflicts", "--json"]);
    assert_eq!(
        conflicts["data"]["conflicts"][0]["resource"],
        "file:crates/rally-core/src/query.rs"
    );
    assert_eq!(
        conflicts["data"]["conflicts"][0]["owners"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let diagnosis = workspace.json(&["diagnose", "--json"]);
    assert!(
        diagnosis["data"]["diagnosis"]["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["code"] == "claim-conflict")
    );

    workspace.json(&[
        "release",
        "--json",
        "--tool",
        "codex",
        "--reason",
        "handed off to pi",
        codex_claim_id,
    ]);
    let cleared = workspace.json(&["conflicts", "--json"]);
    workspace.cleanup();

    assert_eq!(cleared["data"]["conflicts"].as_array().unwrap().len(), 0);
}

#[test]
fn signed_handoff_can_be_synced_to_another_workspace_and_acked() {
    let source = RallyWorkspace::new("journey-sync-source");
    let destination = RallyWorkspace::new("journey-sync-destination");
    let identity_dir = temp_path("journey-sync-identity");
    let identity_arg = identity_dir.to_str().unwrap();

    source.json(&[
        "identity",
        "init",
        "--json",
        "--identity-dir",
        identity_arg,
        "--tool",
        "codex",
    ]);
    let handoff = source.json(&[
        "handoff",
        "--json",
        "--identity-dir",
        identity_arg,
        "--sign",
        "--to",
        "pi",
        "--from-tool",
        "codex",
        "--subject",
        "review signed sync packet",
    ]);
    let handoff_id = handoff["event_id"].as_str().unwrap();

    let packet = source.json(&["sync", "export", "--json"]);
    let packet_path = write_json("journey-sync-packet", &packet);
    let trust_path = identity_dir.join("trust.toml");

    let imported = destination.json(&[
        "sync",
        "import",
        "--json",
        "--trust-policy",
        trust_path.to_str().unwrap(),
        packet_path.to_str().unwrap(),
    ]);
    assert_eq!(imported["data"]["trust_counts"]["trusted"], 1);

    let inbox = destination.json(&["inbox", "--json", "--tool", "pi"]);
    assert_eq!(inbox["data"]["pending"][0]["event_id"], handoff_id);
    assert_eq!(inbox["data"]["pending"][0]["origin"], "import:sync");
    assert_eq!(inbox["data"]["pending"][0]["trust_status"], "trusted");

    destination.json(&[
        "ack",
        "--json",
        "--tool",
        "pi",
        "--summary",
        "packet reviewed",
        handoff_id,
    ]);
    let cleared = destination.json(&["preflight", "--json", "--tool", "pi"]);

    fs::remove_file(packet_path).unwrap();
    source.cleanup();
    destination.cleanup();
    fs::remove_dir_all(identity_dir).unwrap();

    assert_eq!(cleared["pending_acks_for_me"].as_array().unwrap().len(), 0);
}

#[test]
fn rally_watch_wakes_within_a_second_of_an_append() {
    // intent: kqueue/inotify-driven watch must react within ~1s of an append, not on a slow poll cycle.
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    let workspace = RallyWorkspace::new("journey-watch-latency");

    let mut child = Command::new(env!("CARGO_BIN_EXE_rally"))
        .current_dir(&workspace.cwd)
        .env("HOME", &workspace.home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("RALLY_CMUX_CONFIG_DIR")
        .env_remove("RALLY_HERDR_CONFIG_DIR")
        .args(["watch", "--kind", "handoff", "--max-seconds", "5"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    // Let the watcher register before we append.
    thread::sleep(Duration::from_millis(200));

    let append_at = Instant::now();
    workspace.json(&[
        "handoff",
        "--json",
        "--to",
        "claude",
        "--from-tool",
        "codex",
        "--subject",
        "latency probe",
    ]);

    // Read exactly one line of stdout (which is what notify should produce
    // almost immediately) and measure how long it took.
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let observed_after = append_at.elapsed();

    child.wait().ok();
    workspace.cleanup();

    assert!(
        !line.trim().is_empty(),
        "watcher should emit the appended event"
    );
    assert!(
        observed_after < Duration::from_millis(1000),
        "watcher took {observed_after:?} to react; expected < 1s with notify"
    );
}

#[test]
fn rally_watch_emits_new_matching_events() {
    // intent: blocking watchers see only events that arrive after they start (or post-cursor).
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::Duration;

    let workspace = RallyWorkspace::new("journey-watch");

    // Pre-existing event before watch starts.
    workspace.json(&[
        "handoff",
        "--json",
        "--to",
        "claude",
        "--from-tool",
        "codex",
        "--subject",
        "before watch",
    ]);

    // Launch rally watch in the background with a short max-seconds.
    let mut child = Command::new(env!("CARGO_BIN_EXE_rally"))
        .current_dir(&workspace.cwd)
        .env("HOME", &workspace.home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("RALLY_CMUX_CONFIG_DIR")
        .env_remove("RALLY_HERDR_CONFIG_DIR")
        .args([
            "watch",
            "--tool",
            "codex",
            "--kind",
            "handoff",
            "--max-seconds",
            "3",
        ])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    // Give the watcher a moment to record current EOF as its start offset.
    thread::sleep(Duration::from_millis(250));

    // Post a new event after the watcher has started.
    let posted = workspace.json(&[
        "handoff",
        "--json",
        "--to",
        "claude",
        "--from-tool",
        "codex",
        "--subject",
        "after watch starts",
    ]);
    let new_event_id = posted["event_id"].as_str().unwrap().to_string();

    let stdout = child.stdout.take().unwrap();
    let reader = BufReader::new(stdout);
    let lines: Vec<String> = reader.lines().map_while(Result::ok).collect();
    child.wait().ok();

    // Watcher should have emitted exactly the new event, not the pre-existing one.
    let emitted: Vec<Value> = lines
        .iter()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    assert!(
        emitted.iter().any(|e| {
            e.get("event")
                .and_then(|ev| ev.get("id"))
                .and_then(Value::as_str)
                == Some(new_event_id.as_str())
        }),
        "watcher should have emitted the new event, got {emitted:?}"
    );
    assert!(
        emitted.iter().all(|e| {
            e.get("event")
                .and_then(|ev| ev.get("subject"))
                .and_then(Value::as_str)
                != Some("before watch")
        }),
        "watcher should NOT emit pre-existing events without --from-start"
    );

    workspace.cleanup();
}

#[test]
fn inbox_since_cursor_advances_and_filters_per_session() {
    // intent: two sessions of the same tool must each see only what's new since *their* last read.
    let workspace = RallyWorkspace::new("journey-cursor-inbox");

    // Three handoffs to claude over time.
    let h1 = workspace.json(&[
        "handoff",
        "--json",
        "--to",
        "claude",
        "--from-tool",
        "codex",
        "--subject",
        "first",
    ]);
    let h2 = workspace.json(&[
        "handoff",
        "--json",
        "--to",
        "claude",
        "--from-tool",
        "codex",
        "--subject",
        "second",
    ]);

    // Session A reads with cursor — sees both, advances cursor.
    let a_first = workspace.json(&[
        "inbox",
        "--json",
        "--tool",
        "claude",
        "--session-id",
        "session-A",
        "--since-cursor",
    ]);
    assert_eq!(a_first["data"]["pending"].as_array().unwrap().len(), 2);
    assert_eq!(a_first["data"]["cursor_advanced"], true);

    // Session A immediately reads again — nothing new since cursor.
    let a_second = workspace.json(&[
        "inbox",
        "--json",
        "--tool",
        "claude",
        "--session-id",
        "session-A",
        "--since-cursor",
    ]);
    assert_eq!(a_second["data"]["pending"].as_array().unwrap().len(), 0);

    // A third handoff arrives.
    let h3 = workspace.json(&[
        "handoff",
        "--json",
        "--to",
        "claude",
        "--from-tool",
        "codex",
        "--subject",
        "third",
    ]);

    // Session A sees only the new one.
    let a_third = workspace.json(&[
        "inbox",
        "--json",
        "--tool",
        "claude",
        "--session-id",
        "session-A",
        "--since-cursor",
    ]);
    let a_pending = a_third["data"]["pending"].as_array().unwrap();
    assert_eq!(a_pending.len(), 1);
    assert_eq!(a_pending[0]["event_id"], h3["event_id"]);

    // Session B is fresh — sees ALL three handoffs.
    let b_first = workspace.json(&[
        "inbox",
        "--json",
        "--tool",
        "claude",
        "--session-id",
        "session-B",
        "--since-cursor",
    ]);
    assert_eq!(b_first["data"]["pending"].as_array().unwrap().len(), 3);

    // --peek does not advance: session B reading again sees the same set, not empty.
    let b_peek = workspace.json(&[
        "inbox",
        "--json",
        "--tool",
        "claude",
        "--session-id",
        "session-B",
        "--since-cursor",
        "--peek",
    ]);
    assert_eq!(b_peek["data"]["pending"].as_array().unwrap().len(), 0);
    // ^ session B's previous (non-peek) read already advanced. After peek-on-empty, still empty.

    let _ = (h1, h2);
    workspace.cleanup();
}

#[test]
fn rally_post_writes_arbitrary_event_kind() {
    // intent: extensibility — agents can record event kinds outside the typed 13 without forking the binary.
    let workspace = RallyWorkspace::new("journey-post-custom-kind");

    let posted = workspace.json(&[
        "post",
        "--json",
        "--tool",
        "claude",
        "--kind",
        "experiment-result",
        "--payload",
        r#"{"score":0.87,"variant":"A"}"#,
        "--subject",
        "ran eval set Q3",
    ]);

    assert_eq!(posted["kind"], "experiment-result");
    assert_eq!(posted["event"]["type"], "agent-rally.experiment-result.v1");
    assert_eq!(posted["event"]["payload"]["score"], 0.87);
    assert_eq!(posted["event"]["payload"]["variant"], "A");

    // The new event participates in the standard read surface.
    let replay = workspace.json(&["replay", "--json"]);
    let events = replay["data"]["events"].as_array().unwrap();
    assert!(
        events
            .iter()
            .any(|e| e["event"]["kind"] == "experiment-result"),
        "replay should include the custom-kind event: {events:?}"
    );

    workspace.cleanup();
}

#[test]
fn rally_post_rejects_invalid_payload_json() {
    // intent: malformed payload JSON is rejected at the CLI boundary with a clear error, not silently accepted.
    let workspace = RallyWorkspace::new("journey-post-invalid-payload");

    let output = workspace.run(&[
        "post",
        "--json",
        "--tool",
        "claude",
        "--kind",
        "experiment-result",
        "--payload",
        "{not valid json",
    ]);

    assert!(!output.status.success(), "post should fail on bad JSON");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let err: Value = serde_json::from_str(stderr.trim()).expect("error should be JSON");
    assert_eq!(err["ok"], false);
    assert!(
        err["error"]
            .as_str()
            .unwrap()
            .contains("must be valid JSON")
    );

    workspace.cleanup();
}

#[test]
fn rally_post_rejects_non_object_payload() {
    // intent: payloads must be JSON objects so consumers can rely on `payload` being structured.
    let workspace = RallyWorkspace::new("journey-post-non-object-payload");

    let output = workspace.run(&[
        "post",
        "--json",
        "--tool",
        "claude",
        "--kind",
        "experiment-result",
        "--payload",
        "[1,2,3]",
    ]);

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let err: Value = serde_json::from_str(stderr.trim()).expect("error should be JSON");
    assert_eq!(err["ok"], false);
    assert!(err["error"].as_str().unwrap().contains("JSON object"));

    workspace.cleanup();
}

// -----------------------------------------------------------------------------
// Codex installer: TOML-safe `[features]` handling
//
// Regression coverage for the duplicate-`[features]` parse error reported on
// real Codex configs that already carry `[features]\nmemories = true`. The
// installer must mutate the existing `[features]` table instead of emitting a
// second header.
// -----------------------------------------------------------------------------

fn parse_codex_config(path: &std::path::Path) -> toml::Value {
    let text = fs::read_to_string(path).expect("config.toml must be readable");
    toml::from_str(&text).unwrap_or_else(|err| {
        panic!("Codex config must parse as valid TOML, got error: {err}\nfile:\n{text}")
    })
}

fn count_features_headers(path: &std::path::Path) -> usize {
    let text = fs::read_to_string(path).expect("config.toml must be readable");
    text.lines()
        .filter(|line| line.trim() == "[features]")
        .count()
}

fn assert_run_ok(output: std::process::Output) {
    assert!(
        output.status.success(),
        "command failed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn codex_install_preserves_existing_features_table() {
    let workspace = RallyWorkspace::new("codex-install-existing-features");
    let codex_dir = workspace.home.join(".codex");
    fs::create_dir_all(&codex_dir).unwrap();
    let config_path = codex_dir.join("config.toml");
    fs::write(
        &config_path,
        "model = \"gpt-5\"\n\n[features]\nmemories = true\n\n[tools]\nweb_search = true\n",
    )
    .unwrap();

    assert_run_ok(workspace.run(&["setup", "install", "codex", "--json"]));

    let doc = parse_codex_config(&config_path);
    assert_eq!(
        count_features_headers(&config_path),
        1,
        "exactly one [features] header"
    );
    let features = doc["features"]
        .as_table()
        .expect("features must be a table");
    assert_eq!(features["memories"].as_bool(), Some(true));
    assert_eq!(features["hooks"].as_bool(), Some(true));
    // Sibling top-level tables must survive untouched.
    assert_eq!(doc["tools"]["web_search"].as_bool(), Some(true));
    assert_eq!(doc["model"].as_str(), Some("gpt-5"));

    workspace.cleanup();
}

#[test]
fn codex_install_creates_features_when_absent() {
    let workspace = RallyWorkspace::new("codex-install-empty-config");
    let codex_dir = workspace.home.join(".codex");
    fs::create_dir_all(&codex_dir).unwrap();
    let config_path = codex_dir.join("config.toml");
    // Not creating the file at all — installer must handle the missing-file
    // case identically to an empty file.

    assert_run_ok(workspace.run(&["setup", "install", "codex", "--json"]));

    let doc = parse_codex_config(&config_path);
    assert_eq!(count_features_headers(&config_path), 1);
    assert_eq!(doc["features"]["hooks"].as_bool(), Some(true));

    workspace.cleanup();
}

#[test]
fn codex_install_uninstall_round_trip_preserves_siblings() {
    let workspace = RallyWorkspace::new("codex-install-uninstall-roundtrip");
    let codex_dir = workspace.home.join(".codex");
    fs::create_dir_all(&codex_dir).unwrap();
    let config_path = codex_dir.join("config.toml");
    fs::write(
        &config_path,
        "[features]\nmemories = true\n\n[mcp_servers.example]\ncommand = \"echo\"\n",
    )
    .unwrap();

    assert_run_ok(workspace.run(&["setup", "install", "codex", "--json"]));
    let after_install = parse_codex_config(&config_path);
    assert_eq!(after_install["features"]["memories"].as_bool(), Some(true));
    assert_eq!(after_install["features"]["hooks"].as_bool(), Some(true));

    assert_run_ok(workspace.run(&["setup", "uninstall", "codex", "--json"]));
    let after_uninstall = parse_codex_config(&config_path);
    let features = after_uninstall["features"]
        .as_table()
        .expect("features table survives uninstall");
    assert_eq!(features.get("hooks"), None, "features.hooks removed");
    assert_eq!(
        features["memories"].as_bool(),
        Some(true),
        "sibling features.memories preserved"
    );
    assert!(
        after_uninstall["mcp_servers"]["example"]["command"]
            .as_str()
            .is_some(),
        "unrelated table preserved"
    );
    // No orphan rally markers.
    let text = fs::read_to_string(&config_path).unwrap();
    assert!(
        !text.contains("BEGIN rally codex hooks") && !text.contains("END rally codex hooks"),
        "no orphan rally marker block"
    );

    workspace.cleanup();
}

#[test]
fn codex_uninstall_drops_empty_features_table() {
    let workspace = RallyWorkspace::new("codex-uninstall-empty-features");
    let codex_dir = workspace.home.join(".codex");
    fs::create_dir_all(&codex_dir).unwrap();
    let config_path = codex_dir.join("config.toml");
    // No pre-existing [features] table at all.
    fs::write(&config_path, "model = \"gpt-5\"\n").unwrap();

    assert_run_ok(workspace.run(&["setup", "install", "codex", "--json"]));
    assert_eq!(count_features_headers(&config_path), 1);

    assert_run_ok(workspace.run(&["setup", "uninstall", "codex", "--json"]));
    let after = parse_codex_config(&config_path);
    assert!(
        after.get("features").is_none(),
        "empty [features] table removed: {after:?}"
    );
    assert_eq!(after["model"].as_str(), Some("gpt-5"));

    workspace.cleanup();
}

#[test]
fn codex_install_heals_pre_fix_duplicate_features_state() {
    // Reproduces the corrupted on-disk state the pre-fix installer left when
    // it ran against a config that already had `[features]`. Until the heal
    // path runs, this file fails `toml::from_str` with a duplicate-key error.
    let workspace = RallyWorkspace::new("codex-install-heal-duplicate");
    let codex_dir = workspace.home.join(".codex");
    fs::create_dir_all(&codex_dir).unwrap();
    let config_path = codex_dir.join("config.toml");
    let corrupted = concat!(
        "model = \"gpt-5\"\n",
        "\n",
        "[features]\n",
        "memories = true\n",
        "\n",
        "# BEGIN rally codex hooks\n",
        "[features]\n",
        "hooks = true\n",
        "# END rally codex hooks\n",
    );
    fs::write(&config_path, corrupted).unwrap();
    // Sanity-check the precondition — corrupted file must NOT parse.
    assert!(
        toml::from_str::<toml::Value>(&fs::read_to_string(&config_path).unwrap()).is_err(),
        "precondition: corrupted config must fail to parse"
    );

    assert_run_ok(workspace.run(&["setup", "install", "codex", "--json"]));

    let doc = parse_codex_config(&config_path);
    assert_eq!(
        count_features_headers(&config_path),
        1,
        "duplicate [features] headers collapsed"
    );
    let features = doc["features"].as_table().unwrap();
    assert_eq!(features["memories"].as_bool(), Some(true));
    assert_eq!(features["hooks"].as_bool(), Some(true));
    assert_eq!(doc["model"].as_str(), Some("gpt-5"));
    let text = fs::read_to_string(&config_path).unwrap();
    assert!(
        !text.contains("BEGIN rally codex hooks") && !text.contains("END rally codex hooks"),
        "rally marker block stripped during heal"
    );

    workspace.cleanup();
}
