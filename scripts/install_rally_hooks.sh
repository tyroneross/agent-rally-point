#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
#
# install_rally_hooks.sh — idempotent installer for the agent-rally-point
# coordination hook into a coding host's settings.
#
# What it does (Claude Code):
#   Writes/merges the SessionStart + PreToolUse(Edit|Write|MultiEdit) entries
#   in ~/.claude/settings.json so the host fires
#   hooks/rally-coordination-hook.sh automatically. The hook self-gates on
#   missing .rally/, so it is safe to install globally.
#
# Optional (Codex parity):
#   With --repoint-codex, backs up ~/.codex/rally-hook.sh to .bak and replaces
#   it with a thin shim that exec's the in-repo versioned hook. Closes the
#   "loose file desyncs from CLI" recurrence risk documented in
#   docs/assessment-2026-05-31-codex-hook-desync.md.
#
# Idempotent:
#   - A re-run with no changes prints "no change."
#   - --uninstall removes only the entries this installer wrote (matched by
#     command-substring); other hooks are left alone.
#   - --dry-run shows the diff without writing.
#
# Usage:
#   scripts/install_rally_hooks.sh                  # install for Claude Code
#   scripts/install_rally_hooks.sh --repoint-codex  # also repoint ~/.codex/rally-hook.sh
#   scripts/install_rally_hooks.sh --uninstall      # remove
#   scripts/install_rally_hooks.sh --dry-run        # show diff, write nothing
#   scripts/install_rally_hooks.sh --help

set -euo pipefail

# --- args ------------------------------------------------------------------
ACTION="install"
REPOINT_CODEX=0
DRY_RUN=0
QUIET=0

usage() {
  sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
  case "$1" in
    --uninstall) ACTION="uninstall"; shift ;;
    --install)   ACTION="install"; shift ;;
    --repoint-codex) REPOINT_CODEX=1; shift ;;
    --dry-run)   DRY_RUN=1; shift ;;
    --quiet|-q)  QUIET=1; shift ;;
    --help|-h)   usage; exit 0 ;;
    *) echo "install_rally_hooks: unknown arg: $1" >&2; usage >&2; exit 2 ;;
  esac
done

say() { [ "$QUIET" = "1" ] || printf '%s\n' "$*"; }
say_changed() { printf '%s\n' "$*"; }

# --- resolve paths ---------------------------------------------------------
# Locate the repo root (parent of scripts/). Absolute path: Claude Code does
# not reliably expand ~ in command strings.
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd -P)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd -P)"
HOOK_PATH="$REPO_ROOT/hooks/rally-coordination-hook.sh"

if [ ! -x "$HOOK_PATH" ]; then
  echo "install_rally_hooks: missing or non-executable hook at $HOOK_PATH" >&2
  exit 1
fi

CLAUDE_SETTINGS="$HOME/.claude/settings.json"
CODEX_HOOK="$HOME/.codex/rally-hook.sh"

# --- json engine: jq preferred, python3 fallback ---------------------------
if command -v jq >/dev/null 2>&1; then
  JSON_ENGINE="jq"
elif command -v python3 >/dev/null 2>&1; then
  JSON_ENGINE="python3"
else
  echo "install_rally_hooks: need jq or python3 to edit settings.json" >&2
  exit 1
fi

# Marker commands — identify-by-substring on uninstall.
HOOK_CMD_START="$HOOK_PATH start claude_code"
HOOK_CMD_PRETOOL="$HOOK_PATH before-write claude_code"

# --- read existing settings -----------------------------------------------
mkdir -p "$(dirname "$CLAUDE_SETTINGS")"
OLD_JSON=""
if [ -f "$CLAUDE_SETTINGS" ]; then
  OLD_JSON="$(cat "$CLAUDE_SETTINGS")"
else
  OLD_JSON="{}"
fi

# --- compute new settings --------------------------------------------------
compute_new_jq() {
  # $1 = action (install|uninstall)
  local action="$1"
  if [ "$action" = "install" ]; then
    printf '%s' "$OLD_JSON" | jq \
      --arg start_cmd "$HOOK_CMD_START" \
      --arg pretool_cmd "$HOOK_CMD_PRETOOL" \
      --arg matcher "Edit|Write|MultiEdit" \
      '
      # Ensure hooks object
      .hooks //= {}
      # SessionStart
      | .hooks.SessionStart //= []
      | (.hooks.SessionStart |= (
          # Remove any existing rally-coordination-hook entries (idempotency)
          map(.hooks //= [] | .hooks |= map(select(.command // "" | contains("rally-coordination-hook.sh start") | not)))
          | map(select(.hooks | length > 0))
          # Append our entry
          + [ { "hooks": [ { "type": "command", "command": $start_cmd } ] } ]
        ))
      # PreToolUse
      | .hooks.PreToolUse //= []
      | (.hooks.PreToolUse |= (
          map(.hooks //= [] | .hooks |= map(select(.command // "" | contains("rally-coordination-hook.sh before-write") | not)))
          | map(select(.hooks | length > 0))
          + [ { "matcher": $matcher, "hooks": [ { "type": "command", "command": $pretool_cmd } ] } ]
        ))
      '
  else
    # uninstall: strip any rally-coordination-hook entries; drop empty arrays.
    printf '%s' "$OLD_JSON" | jq '
      if .hooks then
        .hooks.SessionStart = (
          (.hooks.SessionStart // [])
          | map(.hooks //= [] | .hooks |= map(select(.command // "" | contains("rally-coordination-hook.sh") | not)))
          | map(select(.hooks | length > 0))
        )
        | .hooks.PreToolUse = (
          (.hooks.PreToolUse // [])
          | map(.hooks //= [] | .hooks |= map(select(.command // "" | contains("rally-coordination-hook.sh") | not)))
          | map(select(.hooks | length > 0))
        )
        | (if (.hooks.SessionStart | length) == 0 then del(.hooks.SessionStart) else . end)
        | (if (.hooks.PreToolUse  | length) == 0 then del(.hooks.PreToolUse)  else . end)
        | (if (.hooks | length) == 0 then del(.hooks) else . end)
      else . end
    '
  fi
}

compute_new_python() {
  local action="$1"
  ACTION_ENV="$action" \
  START_CMD="$HOOK_CMD_START" \
  PRETOOL_CMD="$HOOK_CMD_PRETOOL" \
  MATCHER="Edit|Write|MultiEdit" \
  OLD_JSON_ENV="$OLD_JSON" \
  python3 - <<'PY'
import json, os, sys

old_raw = os.environ.get("OLD_JSON_ENV", "{}")
try:
    data = json.loads(old_raw) if old_raw.strip() else {}
except json.JSONDecodeError as e:
    print(f"install_rally_hooks: existing settings.json is invalid JSON: {e}", file=sys.stderr)
    sys.exit(1)
if not isinstance(data, dict):
    print("install_rally_hooks: settings.json root must be an object", file=sys.stderr)
    sys.exit(1)

action = os.environ["ACTION_ENV"]
start_cmd = os.environ["START_CMD"]
pretool_cmd = os.environ["PRETOOL_CMD"]
matcher = os.environ["MATCHER"]

MARKER = "rally-coordination-hook.sh"

def strip(events, sub):
    """Remove inner hooks whose command contains `sub`; drop empty groups."""
    out = []
    for group in events or []:
        if not isinstance(group, dict):
            out.append(group); continue
        inner = group.get("hooks") or []
        new_inner = [h for h in inner if not (isinstance(h, dict) and sub in (h.get("command") or ""))]
        if new_inner:
            new_group = dict(group)
            new_group["hooks"] = new_inner
            out.append(new_group)
    return out

hooks = data.get("hooks") or {}
if not isinstance(hooks, dict):
    hooks = {}

if action == "install":
    ss = strip(hooks.get("SessionStart"), "rally-coordination-hook.sh start")
    ss.append({"hooks": [{"type": "command", "command": start_cmd}]})
    hooks["SessionStart"] = ss

    pt = strip(hooks.get("PreToolUse"), "rally-coordination-hook.sh before-write")
    pt.append({"matcher": matcher, "hooks": [{"type": "command", "command": pretool_cmd}]})
    hooks["PreToolUse"] = pt

    data["hooks"] = hooks
else:  # uninstall
    ss = strip(hooks.get("SessionStart"), MARKER)
    pt = strip(hooks.get("PreToolUse"), MARKER)
    if ss: hooks["SessionStart"] = ss
    elif "SessionStart" in hooks: del hooks["SessionStart"]
    if pt: hooks["PreToolUse"] = pt
    elif "PreToolUse" in hooks: del hooks["PreToolUse"]
    if hooks:
        data["hooks"] = hooks
    elif "hooks" in data:
        del data["hooks"]

print(json.dumps(data, indent=2))
PY
}

if [ "$JSON_ENGINE" = "jq" ]; then
  NEW_JSON="$(compute_new_jq "$ACTION")"
else
  NEW_JSON="$(compute_new_python "$ACTION")"
fi

# Pretty-print OLD_JSON the same way for clean diffs.
if [ "$JSON_ENGINE" = "jq" ]; then
  OLD_PRETTY="$(printf '%s' "$OLD_JSON" | jq '.')"
else
  OLD_PRETTY="$(OLD_JSON_ENV="$OLD_JSON" python3 -c 'import json,os; print(json.dumps(json.loads(os.environ["OLD_JSON_ENV"] or "{}"), indent=2))')"
fi

# --- compare + write -------------------------------------------------------
CLAUDE_CHANGED=0
if [ "$OLD_PRETTY" != "$NEW_JSON" ]; then
  CLAUDE_CHANGED=1
fi

if [ "$CLAUDE_CHANGED" = "1" ]; then
  say_changed "==> ${ACTION} Claude Code hooks at $CLAUDE_SETTINGS"
  if [ "$DRY_RUN" = "1" ]; then
    say "    (dry-run — settings.json NOT written)"
  else
    # Atomic write via temp file.
    tmp="$(mktemp "${CLAUDE_SETTINGS}.XXXXXX")"
    printf '%s\n' "$NEW_JSON" > "$tmp"
    mv "$tmp" "$CLAUDE_SETTINGS"
  fi
  say "    diff (- old, + new):"
  diff -u <(printf '%s\n' "$OLD_PRETTY") <(printf '%s\n' "$NEW_JSON") | sed 's/^/    /' || true
else
  say "==> Claude Code settings already correct — no change."
fi

# --- optional: repoint ~/.codex/rally-hook.sh -----------------------------
CODEX_CHANGED=0
if [ "$REPOINT_CODEX" = "1" ]; then
  if [ "$ACTION" = "install" ]; then
    mkdir -p "$(dirname "$CODEX_HOOK")"
    SHIM_CONTENT="#!/usr/bin/env bash
# Auto-installed by $REPO_ROOT/scripts/install_rally_hooks.sh
# Delegates to the version-controlled hook so it cannot desync from the CLI.
exec \"$HOOK_PATH\" \"\$@\"
"
    EXISTING=""
    [ -f "$CODEX_HOOK" ] && EXISTING="$(cat "$CODEX_HOOK")"
    if [ "$EXISTING" != "$SHIM_CONTENT" ]; then
      CODEX_CHANGED=1
      say_changed "==> install Codex shim at $CODEX_HOOK -> $HOOK_PATH"
      if [ "$DRY_RUN" = "1" ]; then
        say "    (dry-run — shim NOT written)"
      else
        if [ -f "$CODEX_HOOK" ] && [ ! -f "${CODEX_HOOK}.bak" ]; then
          cp "$CODEX_HOOK" "${CODEX_HOOK}.bak"
          say "    backed up existing codex hook to ${CODEX_HOOK}.bak"
        fi
        printf '%s' "$SHIM_CONTENT" > "$CODEX_HOOK"
        chmod +x "$CODEX_HOOK"
      fi
    else
      say "==> Codex shim already correct — no change."
    fi
  else
    # uninstall: if our shim is in place, restore the .bak (or delete the shim).
    if [ -f "$CODEX_HOOK" ] && grep -q "Auto-installed by .*install_rally_hooks.sh" "$CODEX_HOOK"; then
      CODEX_CHANGED=1
      say_changed "==> uninstall Codex shim at $CODEX_HOOK"
      if [ "$DRY_RUN" = "1" ]; then
        say "    (dry-run — shim NOT removed)"
      elif [ -f "${CODEX_HOOK}.bak" ]; then
        mv "${CODEX_HOOK}.bak" "$CODEX_HOOK"
        say "    restored from ${CODEX_HOOK}.bak"
      else
        rm -f "$CODEX_HOOK"
        say "    removed (no .bak to restore)"
      fi
    else
      say "==> Codex hook is not our shim — leaving alone."
    fi
  fi
fi

# --- summary ---------------------------------------------------------------
if [ "$CLAUDE_CHANGED" = "0" ] && [ "$CODEX_CHANGED" = "0" ]; then
  say "Nothing to do."
fi

if [ "$ACTION" = "install" ] && [ "$DRY_RUN" = "0" ] && [ "$CLAUDE_CHANGED" = "1" ]; then
  say ""
  say "Installed. Hook will fire on Claude Code SessionStart + PreToolUse(Edit|Write|MultiEdit)."
  say "Self-gates outside rally repos (.rally/ absent → exit 0)."
  say "Strict mode (off by default): export RALLY_HOOK_STRICT=1"
  say "Uninstall: $0 --uninstall"
fi

exit 0
