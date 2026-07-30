#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
#
# install_rally_hooks.sh — idempotent installer for the agent-rally-point
# coordination hook into a coding host's settings.
#
# What it does (Claude Code):
#   Writes/merges the SessionStart + UserPromptSubmit + PreToolUse(Edit|Write|MultiEdit) + Stop entries
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
GLOBAL=0

usage() {
  sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
  case "$1" in
    --uninstall) ACTION="uninstall"; shift ;;
    --install)   ACTION="install"; shift ;;
    --repoint-codex) REPOINT_CODEX=1; shift ;;
    --global)    GLOBAL=1; shift ;;
    --dry-run)   DRY_RUN=1; shift ;;
    --quiet|-q)  QUIET=1; shift ;;
    --help|-h)   usage; exit 0 ;;
    *) echo "install_rally_hooks: unknown arg: $1" >&2; usage >&2; exit 2 ;;
  esac
done

# Portable-by-default: the auto-coordination config SHIPS IN THE REPO
# (.claude/settings.json + .codex/hooks.json, committed, using ${CLAUDE_PROJECT_DIR}).
# Opening this repo in Claude Code / Codex auto-loads them — no global change,
# works on any user machine. The global install is an explicit opt-in only.
if [ "$ACTION" = "install" ] && [ "$GLOBAL" != "1" ]; then
  cat >&2 <<'EOF'
install_rally_hooks: nothing to do — project-level config is the portable default
  and is already committed in this repo:
      .claude/settings.json     (Claude Code, via ${CLAUDE_PROJECT_DIR})
      .codex/hooks.json         (Codex, via git-toplevel)
  Open the repo in Claude Code / Codex and trust it on first prompt. Works on any
  machine with NO change to your global ~/.claude or ~/.codex config.

  Opt-in to a USER-WIDE install across every repo on THIS machine (edits your
  global ~/.claude/settings.json — not portable, per-machine):
      scripts/install_rally_hooks.sh --global [--repoint-codex]
  Uninstall the global install:
      scripts/install_rally_hooks.sh --uninstall [--repoint-codex]
EOF
  exit 0
fi

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

# --- generated settings source ---------------------------------------------
TEMPLATE_SETTINGS="$REPO_ROOT/.claude/settings.json"
if [ ! -f "$TEMPLATE_SETTINGS" ]; then
  echo "install_rally_hooks: generated template missing at $TEMPLATE_SETTINGS" >&2
  exit 1
fi
case "${RALLY_INSTALL_JSON_ENGINE:-auto}" in
  auto)
    if command -v python3 >/dev/null 2>&1; then
      JSON_ENGINE=python3
    elif command -v jq >/dev/null 2>&1; then
      JSON_ENGINE=jq
    else
      echo "install_rally_hooks: need python3 or jq to merge generated settings.json" >&2
      exit 1
    fi
    ;;
  python3|jq)
    JSON_ENGINE="$RALLY_INSTALL_JSON_ENGINE"
    if ! command -v "$JSON_ENGINE" >/dev/null 2>&1; then
      echo "install_rally_hooks: requested JSON engine not found: $JSON_ENGINE" >&2
      exit 1
    fi
    ;;
  *)
    echo "install_rally_hooks: invalid RALLY_INSTALL_JSON_ENGINE=$RALLY_INSTALL_JSON_ENGINE" >&2
    exit 2
    ;;
esac

# --- read existing settings -----------------------------------------------
mkdir -p "$(dirname "$CLAUDE_SETTINGS")"
OLD_JSON=""
if [ -f "$CLAUDE_SETTINGS" ]; then
  OLD_JSON="$(cat "$CLAUDE_SETTINGS")"
else
  OLD_JSON="{}"
fi

# --- compute new settings from the generated project template ---------------
compute_new_python() {
  ACTION_ENV="$action" \
  TEMPLATE_SETTINGS_ENV="$TEMPLATE_SETTINGS" \
  HOOK_PATH_ENV="$HOOK_PATH" \
  OLD_JSON_ENV="$OLD_JSON" \
  python3 - <<'PY'
import copy
import json
import os
import sys

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
template_path = os.environ["TEMPLATE_SETTINGS_ENV"]
hook_path = os.environ["HOOK_PATH_ENV"]
MARKER = "rally-coordination-hook.sh"

try:
    template = json.load(open(template_path, encoding="utf-8"))
except (OSError, json.JSONDecodeError) as exc:
    print(f"install_rally_hooks: generated template is invalid: {exc}", file=sys.stderr)
    sys.exit(1)

def strip(events):
    """Remove Rally inner hooks while retaining unrelated hooks/groups."""
    out = []
    for group in events or []:
        if not isinstance(group, dict):
            out.append(group)
            continue
        inner = group.get("hooks") or []
        new_inner = [
            hook
            for hook in inner
            if not (
                isinstance(hook, dict)
                and MARKER in (hook.get("command") or "")
            )
        ]
        if new_inner:
            new_group = dict(group)
            new_group["hooks"] = new_inner
            out.append(new_group)
    return out

hooks = data.get("hooks") or {}
if not isinstance(hooks, dict):
    hooks = {}

if action == "install":
    template_hooks = template.get("hooks") or {}
    if not isinstance(template_hooks, dict):
        print("install_rally_hooks: generated template hooks must be an object", file=sys.stderr)
        sys.exit(1)
    # Remove every prior Rally hook first, including events no longer present in
    # the generated template. Otherwise a retired event stays globally active.
    for event in list(hooks):
        remaining = strip(hooks.get(event))
        if remaining:
            hooks[event] = remaining
        else:
            del hooks[event]
    for event, groups in template_hooks.items():
        installed_groups = copy.deepcopy(groups)
        for group in installed_groups:
            for hook in group.get("hooks", []):
                command = hook.get("command")
                if not isinstance(command, str):
                    continue
                command = command.replace(
                    '"${CLAUDE_PROJECT_DIR}/hooks/rally-coordination-hook.sh"',
                    f'"{hook_path}"',
                )
                hook["command"] = command.replace(
                    "RALLY_HOOK_SOURCE=project",
                    "RALLY_HOOK_SOURCE=global",
                    1,
                )
        hooks[event] = hooks.get(event, []) + installed_groups
    data["hooks"] = hooks
else:  # uninstall
    for event in list(hooks):
        remaining = strip(hooks.get(event))
        if remaining:
            hooks[event] = remaining
        else:
            del hooks[event]
    if hooks:
        data["hooks"] = hooks
    elif "hooks" in data:
        del data["hooks"]

print(json.dumps(data, indent=2))
PY
}

compute_new_jq() {
  printf '%s' "$OLD_JSON" | jq \
    --slurpfile template "$TEMPLATE_SETTINGS" \
    --arg action "$action" \
    --arg hook_path "$HOOK_PATH" \
    '
    def strip_rally($events):
      [
        ($events // [])[] |
        if type == "object" then
          . as $group |
          [
            (.hooks // [])[] |
            select((((.command? // "") | contains("rally-coordination-hook.sh"))) | not)
          ] as $inner |
          select(($inner | length) > 0) |
          $group + {hooks: $inner}
        else
          .
        end
      ];
    def rewrite_group:
      .hooks = [
        (.hooks // [])[] |
        if ((.command? // null) | type) == "string" then
          .command = (
            .command
            | split("\"${CLAUDE_PROJECT_DIR}/hooks/rally-coordination-hook.sh\"")
            | join("\"" + $hook_path + "\"")
            | split("RALLY_HOOK_SOURCE=project")
            | join("RALLY_HOOK_SOURCE=global")
          )
        else
          .
        end
      ];
    if type != "object" then
      error("settings.json root must be an object")
    else
      . as $data |
      (($data.hooks // {}) |
        if type == "object" then . else {} end |
        with_entries(.value = strip_rally(.value)) |
        with_entries(select((.value | length) > 0))
      ) as $clean |
      if $action == "install" then
        (($template[0].hooks // {}) |
          if type == "object" then . else error("generated template hooks must be an object") end
        ) as $template_hooks |
        (reduce ($template_hooks | keys_unsorted[]) as $event
          ($clean;
            .[$event] = (
              (.[$event] // []) + ($template_hooks[$event] | map(rewrite_group))
            )
          )) as $hooks |
        $data | if ($hooks | length) > 0 then .hooks = $hooks else del(.hooks) end
      else
        $data | if ($clean | length) > 0 then .hooks = $clean else del(.hooks) end
      end
    end
    '
}

compute_new() {
  if [ "$JSON_ENGINE" = "python3" ]; then
    compute_new_python
  else
    compute_new_jq
  fi
}

action="$ACTION"
NEW_JSON="$(compute_new)"

# Pretty-print OLD_JSON the same way for clean diffs.
if [ "$JSON_ENGINE" = "python3" ]; then
  OLD_PRETTY="$(OLD_JSON_ENV="$OLD_JSON" python3 -c 'import json,os; print(json.dumps(json.loads(os.environ["OLD_JSON_ENV"] or "{}"), indent=2))')"
else
  OLD_PRETTY="$(printf '%s' "$OLD_JSON" | jq '.')"
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
  say "Installed. Hook will fire on Claude Code SessionStart + UserPromptSubmit + PreToolUse(Edit|Write|MultiEdit) + Stop."
  say "Self-gates outside rally repos (.rally/ absent → exit 0)."
  say "Strict mode (off by default): export RALLY_HOOK_STRICT=1"
  say "Uninstall: $0 --uninstall"
fi

exit 0
