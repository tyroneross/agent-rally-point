# agent-rally-watcher — migration status

## What this is
Vendored snapshot of the standalone [agent-rally-watcher](https://github.com/tyroneross/agent-rally-watcher) Python daemon (v0.1.1, Apache-2.0), imported under `tools/` as a **legacy reference**. Full history is preserved in `archive/bundles/agent-rally-watcher.bundle` (verifiable via `git bundle verify`).

## Path lineage

| Era | Mechanism | Status |
|-----|-----------|--------|
| Legacy | Watcher polls `~/.agent-rally-point/apps/<app>/changes.jsonl` (per-host global path) | Source of this daemon |
| Canonical (now) | Repo-local `.rally/log/` segments + `.rally/manifest.json` | Active — see `RALLY.md` |
| Target | Native `rally watch` subcommand in the Rust CLI (`crates/rally-cli`) | Planned — supersedes this daemon |

When `rally watch` ships and reaches feature parity with this daemon (per-consumer filtering, dispatch hooks, structured stream output), the Python tree under `tools/agent-rally-watcher/` will be retired. Until then, it remains as the reference implementation for the watch/dispatch surface.

## Running from this path

```bash
uv run --project tools/agent-rally-watcher pytest tools/agent-rally-watcher/tests
# or, if uv is not available:
python -m pytest tools/agent-rally-watcher/tests
```

The package entry point (`agent-rally-watcher` console script) is defined in `pyproject.toml`; install with `uv pip install -e tools/agent-rally-watcher` if you need the CLI on PATH.

## What was carried over

`src/`, `tests/`, `examples/`, `README.md`, `ARCHITECTURE.md`, `pyproject.toml`, `LICENSE`, `NOTICE`, `REUSE.toml`, `CHANGELOG.md`, `uv.lock`.

Excluded: `.git/`, `.venv/`, `.pytest_cache/`, `__pycache__/`, `*.egg-info/`, `bin/` (regenerable via `uv run`).

## License / attribution

Apache-2.0; original NOTICE preserved alongside the source. The root `NOTICE` of `agent-rally-point` carries an attribution pointer to this subtree.
