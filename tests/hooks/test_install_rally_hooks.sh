#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
#
# Tests for scripts/install_rally_hooks.sh
#
# Run: tests/hooks/test_install_rally_hooks.sh
# Uses a scratch HOME so the user's real ~/.claude/settings.json is never
# touched.

set -u

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd -P)"
INSTALLER="$REPO_ROOT/scripts/install_rally_hooks.sh"

if [ ! -x "$INSTALLER" ]; then
  echo "FAIL: installer missing or not executable at $INSTALLER"
  exit 1
fi

PASS=0
FAIL=0
FAILS=()

ok()  { PASS=$((PASS+1)); printf 'ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); FAILS+=("$1"); printf 'FAIL %s\n' "$1"; [ -n "${2:-}" ] && printf '     %s\n' "$2"; }

scratch_home() {
  local h
  h="$(mktemp -d)"
  mkdir -p "$h/.claude"
  printf '%s\n' "$h"
}

# Helper: read a JSON field with python3.
jget() {
  # $1 = path to json file ; $2 = python expression on `data`
  python3 -c "import json,sys; data=json.load(open(sys.argv[1])); print($2)" "$1" "$2" 2>/dev/null
}

# ----------------------------------------------------------------------
# Test 0: committed project hooks expose the full rally cadence
# ----------------------------------------------------------------------
T="project hook configs include start/idle/before-write/after-write cadence"
if python3 - "$REPO_ROOT/.codex/hooks.json" "$REPO_ROOT/.claude/settings.json" <<'PY'
import json
import sys

codex_path, claude_path = sys.argv[1:3]

def load(path):
    with open(path, "r", encoding="utf-8") as fh:
        return json.load(fh)

def commands(data, event):
    out = []
    for group in data.get("hooks", {}).get(event, []):
        for hook in group.get("hooks", []):
            cmd = hook.get("command")
            if cmd:
                out.append(cmd)
    return out

def require(data, path, event, phase, tool):
    needle = f"{phase} {tool}"
    if not any("rally-coordination-hook.sh" in cmd and needle in cmd for cmd in commands(data, event)):
        raise AssertionError(f"{path}: missing {event} hook with {needle}")

codex = load(codex_path)
claude = load(claude_path)
for event, phase in [
    ("SessionStart", "start"),
    ("UserPromptSubmit", "idle"),
    ("PreToolUse", "before-write"),
    ("Stop", "after-write"),
]:
    require(codex, codex_path, event, phase, "codex")
    require(claude, claude_path, event, phase, "claude_code")
PY
then
  ok "$T"
else
  bad "$T" "committed .codex/.claude project hooks are missing cadence parity"
fi

# ----------------------------------------------------------------------
# Test 1: install from empty — creates the four rally entries
# ----------------------------------------------------------------------
T="install from empty settings.json"
H="$(scratch_home)"
HOME="$H" "$INSTALLER" --global --quiet >/dev/null 2>&1
rc=$?
if [ "$rc" != "0" ]; then bad "$T" "rc=$rc"; else
  settings="$H/.claude/settings.json"
  if [ ! -f "$settings" ]; then bad "$T" "settings.json not created"
  else
    n_ss=$(python3 -c "import json; d=json.load(open('$settings')); print(len(d.get('hooks',{}).get('SessionStart',[])))" 2>/dev/null)
    n_pt=$(python3 -c "import json; d=json.load(open('$settings')); print(len(d.get('hooks',{}).get('PreToolUse',[])))" 2>/dev/null)
    n_ups=$(python3 -c "import json; d=json.load(open('$settings')); print(len(d.get('hooks',{}).get('UserPromptSubmit',[])))" 2>/dev/null)
    n_stop=$(python3 -c "import json; d=json.load(open('$settings')); print(len(d.get('hooks',{}).get('Stop',[])))" 2>/dev/null)
    has_hook=$(grep -c "rally-coordination-hook.sh" "$settings" 2>/dev/null || echo 0)
    # 4 rally entries: SessionStart(start) + UserPromptSubmit(idle) + PreToolUse(before-write) + Stop(after-write)
    if [ "$n_ss" = "1" ] && [ "$n_pt" = "1" ] && [ "$n_ups" = "1" ] && [ "$n_stop" = "1" ] && [ "$has_hook" = "4" ]; then
      ok "$T"
    else
      bad "$T" "n_ss=$n_ss n_pt=$n_pt n_ups=$n_ups n_stop=$n_stop has_hook=$has_hook"
    fi
  fi
fi
rm -rf "$H"

# ----------------------------------------------------------------------
# Test 2: idempotency — second install reports "no change"
# ----------------------------------------------------------------------
T="install is idempotent (2nd run = no change)"
H="$(scratch_home)"
HOME="$H" "$INSTALLER" --global --quiet >/dev/null 2>&1
before="$(cat "$H/.claude/settings.json")"
HOME="$H" "$INSTALLER" --global 2>&1 | grep -q "no change"
rc=$?
after="$(cat "$H/.claude/settings.json")"
if [ "$rc" = "0" ] && [ "$before" = "$after" ]; then
  ok "$T"
else
  bad "$T" "rc=$rc; before==after: $([ "$before" = "$after" ] && echo yes || echo no)"
fi
rm -rf "$H"

# ----------------------------------------------------------------------
# Test 3: install preserves pre-existing unrelated hooks
# ----------------------------------------------------------------------
T="install preserves unrelated SessionStart/PreToolUse hooks"
H="$(scratch_home)"
cat > "$H/.claude/settings.json" <<'EOF'
{
  "hooks": {
    "SessionStart": [
      { "hooks": [ { "type": "command", "command": "/usr/local/bin/some_other_hook.sh start" } ] }
    ],
    "PreToolUse": [
      { "matcher": "Bash", "hooks": [ { "type": "command", "command": "/usr/local/bin/audit_bash.sh" } ] }
    ]
  }
}
EOF
HOME="$H" "$INSTALLER" --global --quiet >/dev/null 2>&1
rc=$?
# Both unrelated hooks must still be present.
keep_other=$(grep -c "some_other_hook.sh" "$H/.claude/settings.json" 2>/dev/null || echo 0)
keep_audit=$(grep -c "audit_bash.sh" "$H/.claude/settings.json" 2>/dev/null || echo 0)
add_rally=$(grep -c "rally-coordination-hook.sh" "$H/.claude/settings.json" 2>/dev/null || echo 0)
if [ "$rc" = "0" ] && [ "$keep_other" = "1" ] && [ "$keep_audit" = "1" ] && [ "$add_rally" = "4" ]; then
  ok "$T"
else
  bad "$T" "rc=$rc keep_other=$keep_other keep_audit=$keep_audit add_rally=$add_rally"
fi
rm -rf "$H"

# ----------------------------------------------------------------------
# Test 4: uninstall round-trip — install then uninstall returns to original
# ----------------------------------------------------------------------
T="uninstall round-trip restores original settings"
H="$(scratch_home)"
cat > "$H/.claude/settings.json" <<'EOF'
{
  "hooks": {
    "SessionStart": [
      { "hooks": [ { "type": "command", "command": "/usr/local/bin/some_other_hook.sh start" } ] }
    ],
    "PreToolUse": [
      { "matcher": "Bash", "hooks": [ { "type": "command", "command": "/usr/local/bin/audit_bash.sh" } ] }
    ]
  }
}
EOF
original_normalized="$(python3 -c "import json,sys; print(json.dumps(json.load(open(sys.argv[1])), indent=2))" "$H/.claude/settings.json")"
HOME="$H" "$INSTALLER" --global --quiet >/dev/null 2>&1
HOME="$H" "$INSTALLER" --uninstall --quiet >/dev/null 2>&1
final_normalized="$(python3 -c "import json,sys; print(json.dumps(json.load(open(sys.argv[1])), indent=2))" "$H/.claude/settings.json")"
if [ "$original_normalized" = "$final_normalized" ]; then
  ok "$T"
else
  bad "$T" "settings drifted across install/uninstall"
  diff <(printf '%s\n' "$original_normalized") <(printf '%s\n' "$final_normalized") | head -10 | sed 's/^/     /'
fi
rm -rf "$H"

# ----------------------------------------------------------------------
# Test 5: uninstall from empty settings is a no-op
# ----------------------------------------------------------------------
T="uninstall is idempotent (nothing to remove → 'no change')"
H="$(scratch_home)"
echo '{}' > "$H/.claude/settings.json"
out="$(HOME="$H" "$INSTALLER" --uninstall 2>&1)"
rc=$?
if [ "$rc" = "0" ] && printf '%s' "$out" | grep -q "no change"; then
  ok "$T"
else
  bad "$T" "rc=$rc out=$out"
fi
rm -rf "$H"

# ----------------------------------------------------------------------
# Test 6: --dry-run does not write
# ----------------------------------------------------------------------
T="--dry-run leaves settings.json untouched"
H="$(scratch_home)"
echo '{}' > "$H/.claude/settings.json"
before_mtime="$(stat -f %m "$H/.claude/settings.json" 2>/dev/null || stat -c %Y "$H/.claude/settings.json" 2>/dev/null)"
sleep 1
HOME="$H" "$INSTALLER" --global --dry-run --quiet >/dev/null 2>&1
after_mtime="$(stat -f %m "$H/.claude/settings.json" 2>/dev/null || stat -c %Y "$H/.claude/settings.json" 2>/dev/null)"
if [ "$before_mtime" = "$after_mtime" ]; then
  ok "$T"
else
  bad "$T" "settings.json was modified during dry-run"
fi
rm -rf "$H"

# ----------------------------------------------------------------------
# Test 7: --repoint-codex creates a delegating shim with .bak backup
# ----------------------------------------------------------------------
T="--repoint-codex installs shim and backs up existing codex hook"
H="$(scratch_home)"
mkdir -p "$H/.codex"
echo "#!/bin/sh
# old content" > "$H/.codex/rally-hook.sh"
chmod +x "$H/.codex/rally-hook.sh"
old_content="$(cat "$H/.codex/rally-hook.sh")"
HOME="$H" "$INSTALLER" --global --repoint-codex --quiet >/dev/null 2>&1
rc=$?
shim="$H/.codex/rally-hook.sh"
bak="$H/.codex/rally-hook.sh.bak"
if [ "$rc" = "0" ] \
   && [ -f "$shim" ] \
   && grep -q "rally-coordination-hook.sh" "$shim" \
   && [ -f "$bak" ] \
   && [ "$(cat "$bak")" = "$old_content" ]; then
  ok "$T"
else
  bad "$T" "rc=$rc shim_exists=$([ -f "$shim" ] && echo y) bak_exists=$([ -f "$bak" ] && echo y)"
fi
rm -rf "$H"

# ----------------------------------------------------------------------
# Test 8: --repoint-codex --uninstall restores from .bak
# ----------------------------------------------------------------------
T="--repoint-codex --uninstall restores .bak"
H="$(scratch_home)"
mkdir -p "$H/.codex"
printf '#!/bin/sh\n# old content\n' > "$H/.codex/rally-hook.sh"
chmod +x "$H/.codex/rally-hook.sh"
old_content="$(cat "$H/.codex/rally-hook.sh")"
HOME="$H" "$INSTALLER" --global --repoint-codex --quiet >/dev/null 2>&1
HOME="$H" "$INSTALLER" --repoint-codex --uninstall --quiet >/dev/null 2>&1
restored="$(cat "$H/.codex/rally-hook.sh" 2>/dev/null || echo MISSING)"
if [ "$restored" = "$old_content" ]; then
  ok "$T"
else
  bad "$T" "restored content differs from original"
fi
rm -rf "$H"

# ----------------------------------------------------------------------
# Test 9: default (no --global) must NOT write global; guides to project config
# ----------------------------------------------------------------------
T="default (no --global) leaves global untouched + guides to project config"
H="$(mktemp -d)"; mkdir -p "$H/.claude"; printf '{"hooks":{}}' > "$H/.claude/settings.json"
out="$(HOME="$H" "$INSTALLER" 2>&1)"
after="$(cat "$H/.claude/settings.json")"
if printf '%s' "$out" | grep -q "project-level config is the portable default" && [ "$after" = '{"hooks":{}}' ]; then
  ok "$T"
else
  bad "$T" "default run must not write global and must print project-level guidance"
fi
rm -rf "$H"

# ----------------------------------------------------------------------
# Summary
# ----------------------------------------------------------------------
echo ""
echo "Passed: $PASS"
echo "Failed: $FAIL"
if [ "$FAIL" -gt 0 ]; then
  for f in "${FAILS[@]}"; do printf '  - %s\n' "$f"; done
  exit 1
fi
exit 0
