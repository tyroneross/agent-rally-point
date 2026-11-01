# Upgrading Rally

Coordinate a pause and stop a running daemon with its existing compatible CLI
before replacing clients. Updating a plugin alone does not update every CLI or
a running daemon.
Protocol conflicts suggest checking releases; ordinary claim conflicts still
require coordination with the owning agent.

## Check the running build

Run this inside each agent host, including Easy Terminal:

```sh
command -v rally
rally version --json
```

New builds report `build_id`, `wire_version`, `executable_path`,
`release_check_url`, and `upgrade_guide_url`. Older builds may report only
the version and build ID. The executable path identifies the process actually
running; a host may put its bundled binary ahead of your normal shell's PATH.
The same package version can have different source revisions and wire versions.

Check [published releases](https://github.com/tyroneross/agent-rally-point/releases/latest)
and their release notes. A local source build may be newer than every published
release; a newer tag is not proof of protocol compatibility. Protocol errors
print the versions they observed and links to these checks. They do not fetch,
install, stop a daemon, or retry a mutation automatically. There is no background
update check.

## Install a verified release

Use a checkout containing the installer for the intended release. It pins
its download to that checkout's `rally-release.json` when present; it does not
unconditionally choose GitHub's latest release.

```sh
./scripts/install-rally.sh --upgrade --dry-run
./scripts/install-rally.sh --upgrade
```

The installer asks before writing and installs `~/.local/bin/rally`. It verifies
the download checksum and GitHub build-provenance attestation before replacing
the executable. A failed attestation leaves the existing binary in place.
A failed final verification restores the previous executable; if restoration
itself fails, the error names the retained backup for manual recovery.
`--upgrade` bypasses the existing-working-binary and cached-success shortcuts;
an ordinary install may simply report that a working binary already exists.
The `--yes` option is for an explicitly authorized noninteractive install.

To build a trusted local checkout instead:

```sh
./scripts/install-rally.sh --source --upgrade --dry-run
./scripts/install-rally.sh --source --upgrade
```

This builds Rust source into the same `~/.local/bin` location, without release
checksum or provenance verification. Bare `cargo install --path crates/rally-cli`
uses Cargo's configured installation root, normally `~/.cargo/bin`; that can
create another copy. Recheck the actual executable path in every host. Refresh
the shell's command cache or restart the host if needed, then explicitly run
`~/.local/bin/rally version --json` to compare it with the host's `rally`.

## Update hosts and coordinate the daemon

Refresh installed plugin integrations separately using the
[host integration instructions](AUTO-COORDINATION-HOOKS.md), then restart the
affected hosts. A binary bundled by another application may require that
application's update process; the Rally installer does not replace it.

If a daemon is running, arrange a brief pause with peers. Finish or reconcile
in-flight writes. Have the room owner stop it using the existing compatible
CLI **before replacing that CLI**:

```sh
rally daemon stop
```

Now update every client's selected executable and the host integrations. Start
the daemon with the new build, then resume peers:

```sh
rally daemon start
rally daemon status --json
```

Protocol versions must match. There is no automatic negotiation between a v7
client and a v9 daemon. Do not stop a shared daemon merely because one stale
client reports a mismatch, and do not delete `.rally`, claims, or history.
For a mutation whose outcome is unknown, query its exact event ID before
deciding whether to repeat it.

## Run without a daemon or hooks

Without a live daemon, CLI commands open the repository store directly under
the ownership lock. With a daemon, clients send requests to its single store
owner. Both use the same canonical `.rally/log` history and derived SQLite
index. An incompatible live daemon is an error; a client must not open a
competing direct writer beside it.

You can leave the daemon stopped after a coordinated shutdown. No daemon is
required for the core CLI workflow. Hooks are also optional: agents can enter,
acknowledge, claim, check before writing, exchange handoffs, and release claims
explicitly. Without hooks, the agent or orchestrator must run those checks;
the CLI cannot stop an agent that bypasses them and writes a file directly.
