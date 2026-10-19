<!-- SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr | SPDX-License-Identifier: Apache-2.0 -->
# Research packet: assigned vs pool queues, leases, revalidation (2026-10-18)

Scope: prior art for rally's named-handoff vs open-queue ranking, claim leases, pre-pickup validity, and overwrite guards. All sources fetched 2026-10-18 via WebFetch/curl unless marked. Tiers: T1 official docs/standards, T2 eng blogs, T3, T4 forums/opinion.

## 1. AWS SQS: visibility timeout, retention, DLQ (T1)

**Mechanism.** A received message stays in the queue but is invisible to other consumers for the visibility timeout (default 30 s, max 12 h). "If you don't delete it before the timeout expires, the message becomes visible again in the queue and can be retrieved by another consumer." `ChangeMessageVisibility` extends the timeout (a heartbeat), or sets it to 0 to release immediately; extensions never reset the 12 h cap. `MessageRetentionPeriod` (default 4 days, 60 s–14 days) deletes by original enqueue time. A redrive policy moves a message to a DLQ once `ReceiveCount` exceeds `maxReceiveCount` (default 10). The DLQ isolates the message for inspection; SQS does not discard it at that point.
Sources: https://docs.aws.amazon.com/AWSSimpleQueueService/latest/SQSDeveloperGuide/sqs-visibility-timeout.html · https://docs.aws.amazon.com/AWSSimpleQueueService/latest/APIReference/API_SetQueueAttributes.html · https://docs.aws.amazon.com/AWSSimpleQueueService/latest/SQSDeveloperGuide/sqs-dead-letter-queues.html

- **Maps to req 4.** A lapsed claim returns to "available". No code path treats an expired invisibility as still held.
- **Maps to req 2/3.** The receive count is a cheap signal that an item keeps getting picked up and dropped. SQS routes such items to a separate place for inspection; it does not silently close them.
- **Design implication.** Claim state = f(now, lease_expires_at), computed at read time, never a stored "active" flag. Explicit release = set expiry to now (VisibilityTimeout 0 analogue). Track `pickup_attempts` per handoff; past a threshold, label it "needs revalidation". Never delete it. Retention-style deletion by age is the pattern we reject.

## 2. Temporal: task queues, sticky execution, activity timeouts (T1)

**Mechanism.** Schedule-To-Start is "the maximum amount of time that is allowed from when an Activity Task is scheduled". Start-To-Close bounds one attempt. Schedule-To-Close bounds the whole execution including retries. Heartbeat Timeout is "the maximum time between Activity Heartbeats", and when it fires "the Activity Task fails and a retry occurs if a Retry Policy dictates it". Temporal advises against setting Schedule-To-Start as a hard limit and recommends "monitoring the `temporal_activity_schedule_to_start_latency` metric" instead. Sticky execution: a worker also polls a sticky queue "which the Temporal Service shares exclusively with that specific Worker". If that worker doesn't start the task within the sticky timeout (default 5 s), the service "reschedules the Workflow Task in the original queue, allowing any Worker to pick it up."
Sources: https://docs.temporal.io/encyclopedia/detecting-activity-failures · https://docs.temporal.io/sticky-execution

- **Maps to req 1.** Sticky queue = named assignment, polled first. The original shared queue = pool. Work moves from the addressed worker to the pool only after a measured wait expires.
- **Maps to req 3.** Pickup latency is a metric to watch, not a deadline that cancels work. This is the owner's p95-triggers-revalidation rule.
- **Maps to req 4.** The heartbeat timeout fails the attempt and makes the work retryable by others. It does not freeze the work as permanently owned.
- **Design implication.** Compute the per-room p95 of schedule-to-start (handoff posted → first claim) from history. Use it only to flag items as "revalidate before claiming". If a named handoff goes unclaimed past the room's p95, it may *also* be shown in the pool as "available, originally for X". Keep the addressee visible.

## 3. Kubernetes Lease + client-go leaderelection (T1)

**Mechanism.** LeaseSpec fields: `holderIdentity` ("the identity of the holder of a current lease"), `acquireTime`, `renewTime` ("when the current holder ... last updated the lease"), `leaseDurationSeconds` ("a duration that candidates for a lease need to wait to force acquire it. This is measured against the time of last observed renewTime"), and `leaseTransitions` ("number of transitions of a lease between holders"). In client-go, "A client needs to wait a full LeaseDuration without observing a change to the record before it can attempt to take over." The implementation tolerates arbitrary clock skew but not arbitrary skew *rate*. `ReleaseOnCancel` releases the lock early, but only once guarded work has completed; otherwise two processes act at once. Kube-apiservers with the same hostname "take over existing Leases using a new holder identity".
Sources: https://kubernetes.io/docs/reference/kubernetes-api/cluster-resources/lease-v1/ · https://pkg.go.dev/k8s.io/client-go/tools/leaderelection · https://kubernetes.io/docs/concepts/architecture/leases/

- **Maps to req 4.** Takeover is allowed only after renewTime + leaseDuration elapses. Before that, a different holderIdentity is refused. A new instance of the same logical holder adopts the lease under a new identity. This is our "later session of the same agent releases or takes over its own claim" case.
- **Design implication.** Store `holder_agent` (stable name), `holder_session` (per-instance), `acquired_at`, `renewed_at`, `lease_seconds`, and `transitions`. Rules:
  1. Expired (now > renewed_at + lease_seconds) → anyone may take over or release; increment transitions.
  2. Unexpired, same `holder_agent`, different session → allow release or adopt, and log it.
  3. Unexpired, different agent → refuse.

  Evaluate expiry with one clock (the rally server/DB time), never the client's clock.

## 4. HTTP ETag / If-Match / 412 (RFC 9110, T1)

**Mechanism.** §8.8.3: an entity tag is "an opaque validator for differentiating between multiple representations of the same resource". §13.1.1: If-Match uses strong comparison "since the client intends this precondition to prevent the method from being applied if there have been any changes to the representation data". It exists "to prevent the 'lost update' problem". On mismatch the server "MUST NOT perform the requested method" and "MAY" return 412. It may instead return 2xx if the change "appears to have already been applied". §15.5.13: 412 lets a client "prevent the request method from being applied if the target resource is in an unexpected state."
Source: https://www.rfc-editor.org/rfc/rfc9110.txt (fetched via curl; sections 8.8.3, 13.1.1, 15.5.13 read directly)

- **Maps to req 6.** The handoff carries per-file content hashes as its entity tags. At pickup, compare the hashes strongly against the current files.
- **Maps to req 3.** The "already applied" branch is our "a later `done` fact exists" case. Report it as likely complete; don't reapply the work.
- **Design implication.** Show a 412 analogue as a *warning state* ("base diverged: 2 of 5 files changed since posting"), not a block, because agents can legitimately merge. Use content hashes (strong), never mtimes (weak).

## 5. git merge-base / `--force-with-lease=<ref>:<expect>` (T1)

**Mechanism.** `merge-base` finds "the best common ancestor(s) between two commits to use in a three-way merge". `--is-ancestor` tests ancestry via exit code. `--force-with-lease=<refname>:<expect>` updates the ref only "if the current value of the remote ref is the expected value. `git push` fails otherwise". The docs warn that the forms *without* an explicit expect value "interact very badly" with background fetches, since the comparison base silently moves. The documented fix is to tag the base explicitly. `--force-if-includes` is a no-op with an explicit expect.
Sources: https://git-scm.com/docs/git-merge-base · https://git-scm.com/docs/git-push

- **Maps to req 6.** Record `base_commit` at posting time, fixed in the handoff and never recomputed from "current HEAD". At pickup, `git diff --name-only <base_commit> HEAD -- <scope files>` lists the drifted files. `merge-base --is-ancestor base HEAD` detects rewritten history: if base is not an ancestor, warn more strongly.
- **Design implication.** An explicit expected value beats an implicit tracking reference. Store both the commit and the file hashes, because the hashes still cover uncommitted edits.

## 6. Ticket aging: actions/stale, Kubernetes triage robot, Zendesk

**actions/stale (T1, repo README).** It "Warns and then closes issues and PRs that have had no activity". Defaults: label after 60 idle days, close 7 days later with reason `not_planned`. Any update removes the label. `days-before-close: -1` means items "will never be closed automatically" (label-only mode). https://github.com/actions/stale

**Kubernetes k8s-triage-robot (T1, test-infra config).** Applies `lifecycle/stale` after 90 d idle, `lifecycle/rotten` 30 d later, and `/close not-planned` 30 d after that. `/remove-lifecycle` resets, and `lifecycle/frozen` opts out. There is a separate *re-triage* path that only comments: critical-urgent issues after 30 d, important-soon after 90 d, others after 1 yr, "should be re-triaged". https://github.com/kubernetes/test-infra/blob/master/config/jobs/kubernetes/sig-k8s-infra/trusted/sig-contribex-k8s-triage-robot.yaml · https://github.com/kubernetes/community/blob/master/contributors/guide/issue-triage.md

**Zendesk (T1 via search summary, not fetched directly; ⚠️).** The standard close-after-solved automation runs 4 days by default, editable up to 28. Pending reminder notifications exist but are inactive by default. Auto-solving pending tickets is a custom pattern; GitLab's handbook runs 7 d notify / 14 d solve. [UNVERIFIED beyond search summary] https://support.zendesk.com/hc/en-us/articles/4408835051546 · https://handbook.gitlab.com/handbook/support/workflows/zendesk-ticket-basics/

**Criticism (T4, opinion; search summaries only, articles not fetched).** Drew DeVault, "GitHub stale bot considered harmful", argues auto-close discards issues that are still valid and discourages reporters. fvsch.com reaches the same conclusion, and Gutenberg/Dgraph threads raise similar concerns. https://drewdevault.com/blog/stalebot/ · https://fvsch.com/stale-bots

- **Maps to req 3.** Every system above uses a *fixed* age cutoff. The criticized step is the auto-*close*, because closing equates inactivity with invalidity. The K8s re-triage comment and stale's `days-before-close: -1` show that label-only operation is a supported mode.
- **Design implication.** Age only moves an item to "revalidate before claiming". Closing requires evidence (a `done` fact, the plan says done, or a human/agent decision). Any claim or revalidation clears the label (`remove-stale-when-updated` analogue). Thresholds come from each room's p95, not a constant such as 60 or 90 days.

## 7. Dedicated vs shared queue priority: Sidekiq (T1, project wiki)

**Mechanism.** Without weights, queues are processed in strict order: "any job in the default queue will be processed only when the critical queue is empty". Weighted queues are sampled in proportion to their weights. Sidekiq "does not support mixing ordered and weighted queue modes". Strict order risks starving lower queues (inferred; the page doesn't say "starvation").
Source: https://github.com/sidekiq/sidekiq/wiki/Advanced-Options

- **Maps to req 1.** Use strict order: named assignments first, pool only when the named list is empty (idle). Starvation is acceptable here because pool items are optional by definition, and the p95 flag surfaces any that sit too long.
- **Celery:** not researched this pass. [UNVERIFIED]

---

## Design changes caused by research

1. **Lease state is computed, never stored (SQS, K8s).** `active = now_server <= renewed_at + lease_seconds`. Expired claims render as "expired (holder X)" and stop blocking. All reads use server time.
2. **Takeover and release rules follow K8s Lease semantics.**
   - Expired → any agent may release or take over; increment `transitions` and log the previous holder.
   - Unexpired, same `holder_agent`, new session → may release or adopt (apiserver "new holder identity" pattern).
   - Unexpired, different agent → refused.
   - Record `holder_session` separately from `holder_agent`.
3. **Explicit release = expiry set to now** (SQS `VisibilityTimeout=0`). Normal end-of-work releases the lease, so takeover doesn't have to wait out the TTL (K8s `ReleaseOnCancel` lesson). Release happens only after the work is committed.
4. **Named items before pool, in strict order (Sidekiq ordered mode, Temporal sticky queue).** Pool items appear only when the agent has no actionable named items. They are labeled "available, unowned; claim before working" and never "handed to you".
5. **Unclaimed named handoff spills to the pool after the room's p95 (Temporal sticky timeout → original queue).** It stays tagged "originally for X". The p95 is computed from this room's posted→claimed history, with a documented fallback when the sample is small (the threshold is TBD; flag as a decision for the owner).
6. **Age triggers revalidation, never closure (Temporal metric-not-timeout, stale-bot criticism, K8s re-triage).** Age > room p95 → label "revalidate before claiming". Any claim or touch clears the label. No auto-close. Closure requires a `done` fact, a plan marked done, or an explicit decision. A done fact is reported as "appears already applied" (RFC 9110 2xx-if-applied branch).
7. **Overwrite guard = If-Match + explicit expect (RFC 9110, `--force-with-lease`).**
   - Each handoff stores a fixed `base_commit` and `{path: sha256}` for its scope files.
   - At pickup, compare the hashes strongly and run `git diff --name-only base..HEAD`.
   - A mismatch shows a 412-analogue **warning** listing the changed files. It doesn't block the claim.
   - If `merge-base --is-ancestor base HEAD` fails, escalate the warning (history was rewritten).
   - Never recompute the base from the current tracking ref.
8. **Drop counter, no DLQ deletion (SQS maxReceiveCount).** Track `pickup_attempts` (claims that expire without a done fact). Past a threshold, add the label "repeatedly abandoned; revalidate". The item stays visible.
9. **One projection function (req 5) [INFERRED, no single source].** Every system above derives state from one record plus the current time. `classify_work(agent, now)` returns `{assigned, pool, held_claims, revalidate}`, both views read it, and a test asserts both views classify one fixture identically.

## Open items
- ❓ Room p95 fallback when the room has few samples: needs an owner decision (e.g. pooled cross-room p95 vs. no flag).
- ⚠️ Zendesk and stale-bot criticism are cited from search summaries only. Celery was not researched.

## What branch `fix/next-priorities-1018` applied (2026-10-18)

| Research item | Applied? | How, or why not |
|---|---|---|
| 1 lease computed from time | Partly | `next` computes expiry from the effective (renewal-folded) lease and stops calling an expired claim active. Whether an expired claim still blocks a peer's write is AGEN-CLAIMS-m53xzgzp1vm7rnfwnd5tk, with a separate lane on `fix/expired-lease-admission-1013`; this branch does not change conflict detection. |
| 2 takeover rules | Narrowed | Expired lease + same tool label + a stamped later session may `release` / `claim.expired`. A different agent still needs the stale-owner window or the reaper. An unexpired claim stays refused to label-sharing sessions (ARP-R-02). The research's "any agent after expiry" was not adopted, because rally's tool labels are unauthenticated and the brief scoped the change to the same tool. |
| 3 release sets expiry to now | Already true | A release closes the claim in the projection. |
| 4 named before pool | Yes | Named handoff 100 > plan 95 > own blocker 90 > own live claim 75 > open queue 70 (50 when it needs revalidation) > lease-expired claim 30. |
| 5 named handoff spills to the pool after p95 | Rejected | The owner rule says obligations addressed to a tool must not silently age out or be rerouted. |
| 6 age triggers revalidation only | Yes | Plus three content signals: scope changed, a later done fact, a linked plan marked done. Nothing auto-closes. |
| 7 If-Match overwrite guard | Yes, warn-only | `base_commit` + `basehash` (FNV-1a 64, the same hash as the existing `claimhash`, rather than sha256: it detects change and is not a security boundary). It warns at first touch and goes quiet after this tool claims the file. No `merge-base --is-ancestor` escalation yet. |
| 8 pickup_attempts | Deferred | No ledger signal for "claimed then abandoned" is recorded today. |
| 9 one projection | Yes | `obligations::named_handoffs` + `open_queue::open_queue_items` feed both `next` and `inbox`. A unit test and an end-to-end test assert that they agree. |

Open item resolved: when a room has fewer than 20 picked-up open-queue items, the fallback is the measured cross-room p95 (208 h over 518 items in 37 rooms, 2026-10-18). Output names the source (`room` or `cross_room_fallback`). The alternative, "no age flag", would leave new rooms with no age signal at all.
