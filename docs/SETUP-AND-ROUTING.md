<!-- SPDX-License-Identifier: Apache-2.0 -->
# Setup and tested agent routes

Use `rally setup` to discover a runtime or request installation. Use `rally
routes` to see which registered agents have a working, recently tested route.
Both commands provide `--json`; humans and agents use the same status rules.
A runtime check and an agent acknowledgement are separate results.

## Permission and immediate setup

```sh
rally setup --component tmux --json
rally setup --component tmux --apply
```

When tmux is missing, interactive setup asks **Install tmux to enable background
terminal agents?** Answering yes runs the shown package-manager plan and then
checks tmux and its required input-guard fields in a temporary isolated server. No response within 30 seconds, no terminal, or a
negative answer leaves installation pending. Already-installed tmux is reused.

An agent first displays the plan and asks the user that same question. After
explicit approval, its host can apply that exact `plan_id`:

```sh
rally setup --component tmux --apply --approve-plan <approved-plan-id> --json
```

The plan ID binds the component, command/source policy, platform, architecture,
destination policy, initial start and login scope. It is an action fingerprint,
not proof of human identity: the host must obtain the user's approval before
passing it. A changed plan is refused. A per-user lock prevents concurrent
installation; a competing caller receives an in-progress refusal and must read
status again. Setup does not silently choose another installer or backend.

Supported automatic tmux installation uses an existing Homebrew on macOS or
apt-get/dnf on Linux, including required package dependencies. Homebrew automatic
update is disabled for this action. Setup does not install a package manager.
Linux elevation uses noninteractive sudo; if additional authorization is needed,
setup reports the failure. A supported package-manager command is an installation
plan, not evidence that a fresh OS installation has been tested.

`rally setup --component coordinator --apply` asks **Enable Rally's background
coordinator to improve multi-agent coordination?** The coordinator is included
in Rally. Approval enables the per-repository service and its later automatic
activation when multiple fresh leases appear. Approval persists for the exact
plan, including after a failed start, so an unchanged retry needs no second
approval. Use `rally daemon stop` to stop a running coordinator. Stopping it does
not revoke that stored activation permission.

No setup command registers a service at login. The coordinator follows its idle
exit policy; a tmux agent server lasts while its sessions remain and does not
survive a reboot. Neither process lifetime guarantees restored agent state.
Login persistence and permission revocation UI are not implemented by this flow.

`rally setup --component ptyd` reports an existing executable or a live owned
endpoint. There is no verified automatic ptyd installer configured, so Rally
reports that limitation instead of downloading an unspecified binary. An explicit
invalid `RALLY_PTYD_BIN` does not fall back to a different binary. Installed ptyd
still requires an explicit backend launch and an agent route test.

Setup receipts are private atomic JSON files in
`$HOME/.local/share/rally/setup` (`RALLY_SETUP_STATE_DIR` overrides it). Repeated
attempts update the receipt for the same plan. Installer/check execution has a
deadline and observes a diagnostic output budget; timeout kills its process group
and reports an unknown outcome. Inspect the installed state before retrying.
Installation is not transactional and a failed installer may have changed files.

## Route discovery and connection testing

```sh
rally routes --json
rally routes --probe <exact-managed-actor> --tool <sender> --timeout-seconds 20 --json
```

A probe is an explicit agent turn. The sender must have the existing directive
authority (lead, self, target invitation, or leaderless bootstrap). Rally checks
that authority before recording the handoff. It resolves one managed session,
binds the handoff to that session's registration and requests a receipt with a
unique nonce, current role and lead epoch. It never automatically resends.

| Route state | Meaning | Next action |
| --- | --- | --- |
| `polling_only` | Agent presence without a registered live transport | Queue work and have the agent run `rally next` |
| `blocked` | Endpoint checks failed or the worker cannot accept more prompts | Fix the reported binding/runtime condition |
| `testing_required` | Transport responds but current receiver proof is missing | Run an authorized connection probe |
| `ready` | Current transport passes and a correlated receiver proof is fresh | Send the intended handoff with `--require-ack` |

A ready route requires both a recorded transport send and a receiver-authored
artifact/receipt bound to the request, exact session, nonce, role and lead epoch.
The receiver must also echo a separate `RALLY_ROUTE_DELIVERY_` challenge from
the live prompt. Only its SHA-256 digest is stored in the handoff; the raw
challenge is added to the actual backend write, never to sender-authored
directives, wake commands or content facts. Ordinary ledger polling therefore
cannot prove the live injection path. Older probes without this digest remain
unverified. Ordinary `inject --text` remains ledgered and cannot substitute for
this internal connection-check path.
Terminal echo, old screen text, sender transport receipts, blockers and unrelated
responses cannot substitute for that receiver proof. Proof expires after five
minutes. Changed registrations, roles, lead epochs, withdrawn evidence and newer
failed probes invalidate it. Every send still runs its normal live checks.
The registration hash identifies stored endpoint metadata; it is not an OS-level
atomic generation guarantee or cryptographic authentication between local users.

The shared projection exposes at most 128 rows with total/omitted counts and
retains an explicitly probed target. Onboarding outputs (`whoami`, `enter`,
`next`, `room`) point to it instead of running model probes at every hook.

`ready` establishes a connection check, **not** a successful build/review return.
`build_return_verified` remains false. A registered CLI session does not establish
access to an editor's separate native GUI conversation. Cursor, Antigravity and
other hosts need their own authorized end-to-end trials before claiming support.
The protocol does not depend on the model provider.

Run the transport regression against the exact candidate binary with installed
stock tmux:

```sh
python3 tests/runtime/test_route_probe_transport.py --binary /absolute/path/to/rally --output /tmp/route-transport.json
```

The consuming receiver must pass; polling-only, missing-ACK, wrong-token and
wrong-session receivers must fail route verification. The test also checks that
the challenge stays out of the sender's ledger before a response. Missing tmux
or a failed receiver control fails the test; it never counts as a passing
negative. These controlled receivers do not establish live model/provider support.

## Current tmux protections

The stock tmux adapter checks the bound pane/process, copy mode, input-disabled
state and synchronized-pane state. It checks mode, input and synchronization again
immediately before writing.
It refuses disabled or synchronized input. Screen capture never upgrades a send
to receipt. This narrows the check/write race; it does not make separate tmux
commands atomic. No tmux source fork or resident control client ships in this
change. Those remain options for a measured follow-up if an atomic guard is
needed.
