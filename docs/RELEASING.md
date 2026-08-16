# v0.2.5 GitHub Publication Playbook

## Decision

`v0.2.5` is a new, immutable release that supersedes the public `v0.2.1`
release. Do not move, delete, or overwrite `v0.2.1`: existing installers and
reproducible investigations may still resolve it.

## What must be updated

The repository has three distinct publication surfaces. They should not be
treated as one interchangeable "GitHub package."

| Surface | Current release mechanism | v0.2.5 action |
|---------|---------------------------|---------------|
| GitHub Release | `release.yml` builds four CLI targets, their SHA256 sidecars, provenance attestations, and generated GitHub release notes. | Push a new `v0.2.5` tag at the frozen release commit; let the workflow create a draft, upload the full asset set, then publish it as the latest release. |
| Host marketplaces and plugin caches | `config/host-integrations.json` plus the CLI version generate Claude/Codex manifests and the bundled Codex artifact. | Regenerate, commit, and parity-check the derived host surfaces; then use the installed-host reconciler after publication. |
| GitHub Packages, if a legacy package exists | No checked-in workflow currently publishes a modern container, legacy Docker, npm, Maven, NuGet, or RubyGems package. `rally-cli` is `publish = false`. | Inventory first, identify the consumer, and publish a new immutable version only when that package is an actual supported install path. Do not invent a registry package merely to mirror the GitHub Release. |

The public [v0.2.1 release](https://github.com/tyroneross/agent-rally-point/releases/tag/v0.2.1) is the current baseline. It is a GitHub Release with CLI assets, not evidence by itself that a GitHub Packages registry package exists.

## Release sequence

1. Freeze the candidate commit. Confirm every source and generated surface says `0.2.5`; run `python3 scripts/generate_host_surfaces.py --check` and `./scripts/check-release-parity.sh`.
2. Run the single local acceptance gate on that exact tree: `./scripts/release-readiness.sh --full`. It runs the Rust quality gate, builds the current release binary, requires the packaged Node suite to execute with zero skips, runs every pre-push contract, checks workflow syntax, and rejects candidate content drift during the run.
3. Commit and merge the frozen candidate through the normal review path. Record the exact commit SHA in the release notes or operator log.
4. Create and push one annotated `v0.2.5` tag at that SHA. Never repoint `v0.2.1`.
5. Watch the Release workflow. It reruns the pinned quality and parity gates, builds all four target triples, verifies all eight binary/checksum files, generates GitHub release notes, stages them in a draft release, and publishes it as latest only after the upload succeeds.
6. Verify the public release has the expected four binaries, four `.sha256` sidecars, and provenance attestations. Then exercise the installer on a clean machine or disposable environment.
7. Reconcile installed Claude/Codex integrations with `python3 scripts/sync_host_integrations.py --apply --json`; restart hosts when the report asks for it.

## GitHub Packages remediation, only if inventory finds one

The current credential can read repository releases but lacks `read:packages`,
so package inventory is currently blocked. Before changing any registry package,
authorize the narrowest needed scope:

```bash
# Read-only inventory first. Do not grant write or delete access until a package
# consumer and publication workflow are confirmed.
gh auth refresh -h github.com -s read:packages
```

Then inventory each supported registry type and map every result to a documented
consumer. A 403, an empty list, and an undiscovered package are different
outcomes; do not treat one as another.

```bash
for type in container docker npm maven nuget rubygems; do
  gh api --paginate "users/tyroneross/packages?package_type=${type}&per_page=100"
done
```

`container` covers GitHub's current Container registry. `docker` inventories
packages retained from the legacy Docker registry; querying both prevents a
stale legacy package from being mistaken for no package.

For a package that is both stale and still consumed:

1. Add an explicit package-specific publisher and reproducible version source; it must emit `0.2.5` from the same frozen commit as the GitHub Release.
2. Grant `write:packages` only to the publishing identity. Never overwrite a prior version; publish a new version and update a `latest`/default tag only after installation verification.
3. Verify installation by exact version, provenance or checksum where the ecosystem supports it, repository linkage, and the consumer's documented install command.
4. Keep the old package version readable until downstream consumers have migrated. Do not use deletion as an update mechanism.

If inventory finds no consumer-facing package, the correct remediation is to
document that the supported artifact is the GitHub Release binary, rather than
publish an unused duplicate registry package.
