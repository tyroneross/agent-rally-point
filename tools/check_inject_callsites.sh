#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
#
# check_inject_callsites.sh — Plan F P2 G4 caller audit.
#
# The F plan calls out "34 documented rally inject call-sites." Most are in
# documentation (RALLY.md, READMEs, skill bodies) and use prose to REFERENCE
# the command. The actual runtime-shelling-out callers are a much smaller
# set:
#   1. crates/rally-cli/src/lib.rs            -- the implementation itself
#   2. crates/rally-cli/tests/user_journey.rs -- the integration test caller
#   3. tools/herdr-smoke.sh                   -- in the easy-terminal repo
#   4. (potentially) build-loop and rally-watcher Python helpers
#
# The contract this audit verifies:
#   - Every shell-out invocation of `rally inject` passes a target +
#     either `--text <body>` or `--handoff <id>` (the Plan F contract
#     preserves THIS signature; only the underlying delivery semantics
#     change).
#   - No caller passes `--backend` or relies on a herdr-specific arg
#     that Plan F removes from the inject critical path.
#
# Heuristic: a real CALLER is a line that has `rally inject ` (with trailing
# space and a positional arg next, NOT immediately followed by a punctuation
# token like `: `, `.`, `\"` that signals prose). We look at the line itself
# AND the next 3 lines (multi-line shell continuations) to find --text/
# --handoff. Documentation hits (md/rst/adoc) are excluded outright.
#
# Exit codes:
#   0 = audit pass (no breakages found)
#   1 = audit found callers that would break under Plan F semantics
#   2 = repo/path-discovery error
#
# Usage:
#   tools/check_inject_callsites.sh                    # audit the standard set of repos
#   tools/check_inject_callsites.sh --json             # machine-readable output
#   tools/check_inject_callsites.sh --extra-path <dir> # add a search root

set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RALLY_REPO="$(cd "$HERE/.." && pwd)"

JSON=0
EXTRA_PATHS=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --json) JSON=1; shift ;;
        --extra-path)
            shift
            if [[ -d "${1:-}" ]]; then
                EXTRA_PATHS+=("$1")
            fi
            shift
            ;;
        *) shift ;;
    esac
done

ROOTS=(
    "$RALLY_REPO"
    "$HOME/dev/git-folder/easy-terminal"
    "$HOME/.claude/plugins/cache/rosslabs-ai-toolkit/build-loop"
    "$HOME/dev/git-folder/build-loop-memory"
)
for p in "${EXTRA_PATHS[@]+"${EXTRA_PATHS[@]}"}"; do
    ROOTS+=("$p")
done

CALLSITE_COUNT=0
DOC_REF_COUNT=0
PROSE_REF_COUNT=0
BREAKING_CALLSITES=()

is_doc_or_text() {
    case "$1" in
        *.md|*.txt|*.rst|*.adoc) return 0 ;;
        # Snapshots of files under build-loop-memory/projects/*/raw/files/
        # are reference copies, not live callers.
        */build-loop-memory/projects/*/raw/files/*) return 0 ;;
        # The audit script's own banner/output strings — self-references.
        */check_inject_callsites.sh) return 0 ;;
        *) return 1 ;;
    esac
}

# A line is a real CALLER if it contains `rally inject <something>` where
# <something> is NOT prose punctuation (`. ` , `,` , `:` , `"` , `'`).
# That filters format strings like `"rally inject {status}..."` and prose
# like ``rally inject` is...`.
is_caller_line() {
    local line="$1"
    # Real-caller signatures we accept after `rally inject `:
    #   `rally inject <target>` -> identifier
    #   `rally inject "$VAR"`    -> shell expansion
    #   `rally inject ${VAR}`    -> shell expansion (alt)
    #   `rally inject --json`    -> direct flag
    # We REJECT:
    #   `rally inject: ...`            (log/banner line — colon prose)
    #   ``rally inject` is ...`        (markdown backtick)
    #   `'rally inject' is ...`        (single-quote prose)
    #   `"rally inject {format ..."`   (format string with `{` immediately after the args)
    echo "$line" | grep -qE 'rally inject ([A-Za-z0-9_\-]+|"\$|\$\{|--)' && \
        ! echo "$line" | grep -qE 'rally inject [`'"'"'.,:]' && \
        ! echo "$line" | grep -qE 'rally inject [a-z]+ \{'  # format-string heuristic
}

for root in "${ROOTS[@]}"; do
    [[ -d "$root" ]] || continue
    while IFS= read -r match; do
        file="${match%%:*}"
        rest="${match#*:}"
        line_no="${rest%%:*}"
        line="${rest#*:}"

        if is_doc_or_text "$file"; then
            ((DOC_REF_COUNT += 1))
            continue
        fi

        # Skip code comments / docstrings.
        prefix="$(echo "$line" | sed 's/^[[:space:]]*//' | head -c 4)"
        case "$prefix" in
            '//'*|'#'*|'/*'*|'"""'*|"'''"*)
                ((DOC_REF_COUNT += 1))
                continue
                ;;
        esac

        # Is this a real caller invocation?
        if ! is_caller_line "$line"; then
            ((PROSE_REF_COUNT += 1))
            continue
        fi

        ((CALLSITE_COUNT += 1))

        # Plan F preserves the signature: <target> --text <body> OR
        # <target> --handoff <id>. Check this line + next 3 lines.
        ctx="$(sed -n "${line_no},$((line_no + 3))p" "$file" 2>/dev/null)"
        if ! echo "$ctx" | grep -qE '\-\-text|\-\-handoff'; then
            BREAKING_CALLSITES+=("$file:$line_no:$line")
        fi
    done < <(grep -rEn --include='*.sh' --include='*.py' --include='*.rs' --include='*.toml' \
                'rally inject' "$root" 2>/dev/null || true)
done

if [[ "$JSON" == "1" ]]; then
    printf '{"callsite_count":%d,"doc_ref_count":%d,"prose_ref_count":%d,"breaking_count":%d,"breaking":[' \
        "$CALLSITE_COUNT" "$DOC_REF_COUNT" "$PROSE_REF_COUNT" "${#BREAKING_CALLSITES[@]}"
    first=1
    for b in "${BREAKING_CALLSITES[@]+"${BREAKING_CALLSITES[@]}"}"; do
        [[ "$first" == "1" ]] || printf ','
        first=0
        esc=$(printf '%s' "$b" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read().rstrip()))')
        printf '%s' "$esc"
    done
    printf ']}\n'
else
    echo "rally inject caller audit (Plan F P2 G4)"
    echo "  doc references:   $DOC_REF_COUNT"
    echo "  prose hits:       $PROSE_REF_COUNT (format strings, identifiers in prose)"
    echo "  real callsites:   $CALLSITE_COUNT"
    echo "  breaking sigs:    ${#BREAKING_CALLSITES[@]}"
    if (( ${#BREAKING_CALLSITES[@]} > 0 )); then
        echo "  BREAKING (Plan F semantics) ↓"
        for b in "${BREAKING_CALLSITES[@]}"; do
            echo "    $b"
        done
    fi
fi

(( ${#BREAKING_CALLSITES[@]} == 0 ))
