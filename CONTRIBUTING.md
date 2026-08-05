<!-- SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com> | SPDX-License-Identifier: Apache-2.0 -->

# Contributing to agent-rally-point

Thanks for your interest. A few load-bearing conventions before you open a PR.

## Commit identity — read this before your first commit

Every commit must carry a real, routable identity in its author and committer fields. Two hooks enforce it: `.githooks/pre-commit` checks the identity before the commit object exists, and `.githooks/pre-push` re-checks every commit you are about to push. Both stay silent when your identity is correct.

Set your identity globally, once:

```bash
git config --global user.name "Your Name"
git config --global user.email "you@example.org"   # an address you actually own
```

Then add yourself to `config/git-identity-allowlist.txt` in your PR. The gate denies by default, because the address that caused this rule was neither obviously good nor obviously bad — it was a test fixture's.

The gate rejects four shapes:

| Rejected | Why |
|---|---|
| `*@example.*`, `*@*.invalid`, `*@*.test` | RFC 2606 reserved. These are test-fixture identities, never a person. |
| `*@*.local`, `*@localhost` | Git's hostname fallback when no identity is configured. It looks like an address and reaches nobody. |
| `noreply@anthropic.com`, `noreply@openai.com` in the author or committer field | Agent identities. See the next section — these belong in a trailer, not in authorship. |
| Anything not on the allowlist | Deny by default. |

**Never set your identity with `git config` inside a test fixture.** `git config` defaults to `--local`, and `--local` resolves to *the repository enclosing the path you named*, not the path itself. A fixture whose root drifted into a real checkout wrote its identity into that checkout's `.git/config`, where it outranked the global one, and 64 commits landed under `Rally Test <rally@example.test>` before anyone noticed. `docs/ROOT-CAUSE-REGISTER.md` RC-064 has the full mechanism.

Fixtures pass identity per invocation instead, and `crates/rally-cli/src/test_git_fixture.rs` is the only place that does it:

```rust
fixture_git(&root, &["commit", "--allow-empty", "-m", "initial"]);
```

That helper supplies `-c user.name=` / `-c user.email=` and the `GIT_AUTHOR_*` / `GIT_COMMITTER_*` environment variables on each call, writes to no config file, and panics if its root is outside the process temp directory. Use it. Do not hand-roll a second copy — the last time this defect was fixed, it was fixed for one config key, and the class reappeared two months later on a key that fix did not cover.

If a commit is refused, the message names the offending address, the correct one, and the exact command to fix it. The usual cause is a repo-local override silently beating your global config:

```bash
git config --local --get user.email      # if this prints anything unexpected:
git config --local --unset user.email
git config --local --unset user.name
```

## License & Attribution

agent-rally-point is licensed under the **Apache License, Version 2.0**. By contributing, you agree your contribution is licensed under the same terms.

Downstream redistribution rules (from the license itself):

- Apache 2.0 **§4(c)** — you must retain, in the source form of any derivative work, all copyright, patent, trademark, and attribution notices from the source form of the work. Translation: don't strip the per-file `SPDX-FileCopyrightText` and `SPDX-License-Identifier` headers when you fork or vendor source files.
- Apache 2.0 **§4(d)** — if the work includes a `NOTICE` file, derivative works you distribute must include a readable copy of the attribution notices it contains. Translation: when you redistribute agent-rally-point, the `NOTICE` file at the repo root must travel with it (in a `NOTICE` file, in your docs, or rendered by the product). The contents of `NOTICE` are informational — they cannot add license terms — but the obligation to preserve them is binding.

Per-file headers in this repo follow REUSE 3.3 (https://reuse.software/spec-3.3/). Files that cannot carry an inline comment (`.json`, binary assets) are annotated via `REUSE.toml` at the repo root. Validate locally with `uvx reuse lint`.

## AI co-author attribution

A significant portion of this codebase was written collaboratively with AI coding assistants — Anthropic's Claude (via Claude Code) and OpenAI's Codex (via Codex CLI). The convention this repo follows: **every commit produced with meaningful AI assistance ends with a Git `Co-Authored-By:` trailer naming the model**.

For Claude Code sessions, the trailer is:

```
Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
```

Substitute the actual model + tier you used (e.g., `Claude Sonnet 4.6`, `Claude Haiku 4.5`).

For Codex CLI sessions, the trailer is:

```
Co-Authored-By: OpenAI Codex <noreply@openai.com>
```

GitHub renders the avatar of any recognized email on the commit page, so the AI contribution is visible at the commit level. This is a community convention, not a legal requirement of Apache 2.0. If you're authoring without AI assistance, omit the trailer; don't pad commits with it.

**The trailer is the only place an agent identity belongs.** Authorship names the human or service accountable for the work, so `noreply@anthropic.com` and `noreply@openai.com` are rejected in the author and committer fields and accepted without comment in a `Co-Authored-By:` trailer. The identity gate reads only the author and committer fields and never parses the commit body, so it cannot flag a trailer no matter what address the trailer carries.

## Signed commits

Signed commits (`git commit -S` for GPG, or SSH-signed via `git config gpg.format ssh`) are **recommended** and surfaced as `Verified` badges by GitHub. They strengthen the evidentiary chain in case of an authorship dispute. They are not enforced.

## Commit message style

Conventional Commits (https://www.conventionalcommits.org/) — `type(scope): subject`. Common types in this repo: `feat`, `fix`, `chore`, `docs`, `refactor`, `test`, `perf`.

## Verification

Rust is the greenfield acceptance path:

```bash
cargo test
cargo clippy --all-targets -- -D warnings
git diff --check
```

Do not run legacy compatibility gates as the default proof for Rust-core
changes. The older package is cutover material and no longer defines the
product contract.
