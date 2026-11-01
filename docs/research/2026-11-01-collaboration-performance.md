# Faster collaboration with the same write authority

Rally now supports one canonical batch reply read, returns pending handoffs without an implicit receiver wait, and wakes explicit waits on filesystem events. SQLite and its existing lookup indexes remain in place. Interactive artifact/status posts retain the existing bounded store-ownership corridor instead of being refused at a hook-sized deadline.

## What the supplied screening establishes

The supplied portable 24-cell screening used Rally `cb0713f` and hcom `de33e31a0cdbba4c5863f7cc050d8b79aa50bcdf` in a Linux sandbox. The local starting point was `4f6ec023be41e5ac96abe444301037d4cf91dfd3`; intervening routing fixes do not invalidate the observer bottleneck but prevent treating the package as a test of the current installed product.

All 13,690 manifest entries matched their SHA-256 hashes, with no missing files or mismatches. The extra part-06 download was identical. Reconstructed totals were 1,014 offered requests per product. The high-load cells offered 674 each: Rally sent 671 and hcom sent 674. The report's 355 Rally requests not observed within the deadline comprise three send refusals and 352 late observations, rather than 355 proven lost tasks.

The Rally sender adapter ran `locate(request)` and then `locate(counted reply)` for each request. In the high-load cells, receiver ACK recording was much earlier than sender observation (reported medians about 736 ms versus 15.7 s). This supports reducing observer work. It does not isolate wire transport, establish provider busy-hook behavior, or prove end-to-end model throughput. The sandbox used record-only delivery, so removing implicit inject waits is a separate improvement.

## Decisions and evidence

| Change or option | Evidence and disposition |
| --- | --- |
| Keep JSONL canonical, SQLite derived | Preserve durable append, fsync/readback, ownership locks and existing authority gates. No storage replacement. |
| Add reference index | Identical rows on a 24,086-event copied database: median SQL-only lookup 13.4479 ms to 0.001687 ms, 36 measured samples after six warmups. Rust EXPLAIN tests require indexed SEARCH. Excludes CLI and transport. |
| Batch canonical reply observation | New `rally replies --ref ID --ref ID --json` includes original, projected state and counted reply from one read. Tests cover original/reply withdrawal, exact sessions, third-party replies, acceptance before completion, quoted/Unicode/missing/duplicate IDs, 1,200 absent IDs and old-daemon fallback. |
| Reject `recent` as an ACK shortcut | It was faster but falsely acknowledged a withdrawn request; a small result cap also omitted IDs. Never substitute it for canonical reply state. |
| Async handoff default | Ordinary inject returns pending; `--wait-ack` and compatible `--require-ack` explicitly wait. Receiver acceptance keeps assigned work open. Caller-selected deadlines remain unchanged. |
| Event-driven explicit wait | A persistent log watcher is registered before the first read; typed receiver checks remain authoritative. Events are hints with the existing 250 ms periodic fallback. |
| One cadence fold | Debug microfixture with 10,000 facts/300 peers: 37.38 ms reference scan versus 3.77 ms one-pass map. Same-tool sequence ties and a latest missing/invalid cadence preserve reference behavior. |
| Completion evidence | Empty/whitespace-only artifact evidence remains resolved-unverified. A later authorized evidenced artifact clears the warning; withdrawing it restores the warning. Evidence presence does not independently verify an external effect. |
| Status-post overload | On a copied 24,095-fact room, six simultaneous default-budget posts produced four writes and two explicit pre-write ownership refusals. With a longer bounded wait all six committed once. Preserve the existing 30-second ownership corridor plus response headroom for interactive artifact posts and manual `status post`; explicit caps and 3-second hook defaults still apply. This improves admission under contention, not write latency. |
| Daemon timeout ambiguity | A transport-error reply after dispatch does not prove noncommit. Treat it like a lost socket reply: query by stable event ID before retrying. Only a typed NotStarted response proves no durable mutation began. |

## Measured limits

Astra independently measured the immutable debug candidate: observing 16 requests across four receivers took a median 1,089.32 ms with serial request/reply locates versus 16.54 ms with canonical batching (three alternating samples, about 65.9 times faster for this observer operation). Withdrawn originals and withdrawn ACKs did not become false ACKs.

An isolated exact-session fixture returned a default pending ledger injection in 151.83 ms; the baseline's explicitly measured one-second implicit wait took 1,791.57 ms. Three paired explicit-wait samples measured receiver ACK CLI return to sender return at medians 140.32 ms before and 5.76 ms after. These are local filesystem fixtures, excluding models and PTY delivery.

Whole-room rendering on a copied 24,095-fact room showed only about 1.5 percent difference (4,263.99 versus 4,200.86 ms, three alternating samples). That is not a demonstrated meaningful whole-room improvement. Warm debug status posts still spend about half a second after the canonical append becomes visible, largely on rich room context; this patch preserves the existing response contract.

The optimized Rally candidate (SHA-256 `9d6bb7b9ae70eca3e5675a5e4659ca3502e05b79cd36ef7b9af28bcf691f9f63`) repeated a fresh 16-request/four-receiver all-ACK fixture: median serial observation 385.76 ms versus batch 9.93 ms over three alternating samples, about 38.8 times faster for observation. A real child rallyd produced identical batch results, including original withdrawal, ACK withdrawal and missing IDs. On a copied 24,095-fact room, 12/12 concurrent status posts (six default, six explicit 30-second) committed exactly once. The earlier refusal reproduction used debug builds; do not attribute the release-versus-debug latency difference to the timeout policy. The deterministic Rust held-lock test isolates that policy change.

Pinned local hcom was built with its locked release configuration in an isolated HOME/HCOM_DIR fixture. Its own supported batched observer also amortized command cost. Its release medians were 186.90 ms serial and 11.42 ms batched, three alternating samples. Rally used Rust 1.95.0 and hcom used its pinned Rust 1.99.0; runs occurred at different times. Profile class matches for the optimized measurements, but compilers, contracts and fixture details differ. These small operational samples do not establish a product-wide winner. No result here establishes exactly-once external effects, safe busy-provider injection, remote transport performance or paid-model task throughput. The screening's effect-before-ACK crash duplicated application effects in both systems; application idempotency remains necessary.

## Verification and provenance

Implementation and regression tests are Rust; no runtime dependency was added. The pre-integration candidate library suite passed 1,182 tests with 19 pre-existing ignored manual/performance tests; selected integration suites passed channel 27, handoff targeting 28, user journey 109, idle wake 25, and concurrent writes 3. The protocol wire tests, formatting and Clippy passed. Tests exercise claim exclusion, exact receiver sessions, canonical retractions, pending/accepted/done distinctions, schema compatibility, legacy fallback, explicit waits and concurrent durable writes. Independent Astra review found the empty-evidence classification gap and rejected the unsafe recent-query shortcut. Both were addressed.

Claude review was attempted once using `claude --bare --print --tools ''`, which skips Keychain, hooks and plugins. It returned Not logged in without an authorized credential path; Claude review is unavailable. No login, credential extraction or Keychain operation was attempted.

Private evidence is retained under `.build-loop/runs/rally-speed-20261101/`: the import receipt and screening audit in `evidence/`, immutable-candidate measurements and review in `astra/`, and the acceptance plan/test logs at the run root. The supplied Claude report of two dropped status posts is reported evidence. A matching refusal class was reproduced locally; the exact original incidents cannot be attributed without their command results.

## Concurrent lookups, edits and merges

Batching is explicitly requested for several event IDs in the current room; it does not automatically batch file ownership checks, claims, edits or merges. Single-ID `locate` retains cross-room discovery; inject's prior-ACK read uses the narrow single reply lookup. Both single and batch lookups retain SQLite indexes. A reply batch can include handoffs scoped to the same file, but their accepted state grants no file ownership. Querying a claim ID returns its original fact with no reply contract; it is not a current ownership check.

The Rust same-file regression runs direct and actual daemon modes: four concurrent observers perform 12 batches while two distinct agents with accepted assignments race to claim `shared.rs`. Exactly one claim commits; the other receives a claim-conflict refusal. Strict edit and coordination checks allow the owner and deny the nonowner. An unauthorized release cannot remove the claim. An authorized release permits reassignment, and the former owner is denied despite its stale earlier observation.

The fixture also creates two real Git branches editing the same line. Even when a caller deliberately ignores the denied Rally check, the second merge must conflict, leave HEAD at the first accepted commit, and preserve both versions in the conflict. Aborting restores the first version. This tests Git's conflict behavior; Rally reply reads never perform or authorize Git merges.

Merge safety still requires callers to enforce the coordination check and provide the complete changed-file list. Validate the actual coordination phase and `pass=true`, rather than treating exit success alone as proof: the existing read-check watchdog may emit an advisory timeout envelope. The merge check verifies tool-level presence, acknowledgment and path coverage; it is not an atomic Git lock, commit-hash check or proof of required branch protection. These existing limits are unchanged by batching.

## Live Rally dogfood

The optimized candidate was exercised in this repository's actual canonical room, reaching sequence 24,190. A real parent/reviewer exchange used handoff `fact_13fb5_18e3e42f00947310`, typed acceptance `fact_155df_18e3e4382f4ff888`, then evidenced completion artifact `fact_17e85_18e3e461b7395590`. Parent batch observations matched individual locate state, kept accepted work open, then reported done/evidenced and removed the completed request from both open handoffs and the receiver's empty inbox. The exact receiver session authored acceptance and completion.

Two live waves of concurrent reads succeeded: four parent reads (14.65–58.48 ms) and eight reviewer reads with four workers (12.83–114.22 ms). These are finite observations in an active room, not a controlled benchmark. The reviewer's competing `RALLY.md` claim returned a claim conflict; its strict edit check exited 4 with allow=false. The file remained unchanged during the probe, and both existing parent-owned claim facts were preserved after completion. No unrelated acknowledgments or source edits were performed by the reviewer.

This dogfood used the candidate binary against live room data; it did not replace the installed CLI or prove PTY wake delivery. The parent also encountered a separate 3-second whoami read watchdog; an explicit 15-second retry returned actionable=true. The batch operations themselves all succeeded. Detailed proof is retained in `live-dogfood-summary.json` and `astra/live-dogfood/` under the private run.

The integrated shared Rust gate passed 2,135 tests with 22 existing ignored tests. The additional same-file regression and its 29-test integration file passed afterward. Generated host/release metadata was refreshed, and release parity passed.
