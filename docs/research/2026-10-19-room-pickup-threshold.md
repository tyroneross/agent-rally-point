<!-- SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr | SPDX-License-Identifier: Apache-2.0 -->
# Per-repo open-queue age threshold (2026-10-19)

The revalidation age signal comes from each repo's own history; no other repo's data enters. A room is one repo, and Operations Center work and release batches are per repo (owner decision, 2026-10-19).

    threshold = max(1 h, this repo's nearest-rank p95 of open-queue pickup time)   when the repo has n >= 8 pickups
    no age signal (pickup_p95_source = "insufficient_history")                        when n < 8

A pickup is the first non-system resolve, receipt, or artifact that refs an original open-queue handoff. The sender closing its own posting does not count. Below N the other revalidation signals (scope changed, later done fact, linked plan done) still apply. The policy lives in `crates/rally-cli/src/pickup_threshold.rs`.

## Reproduce

    python3 -I scripts/research/room_pickup_threshold.py <db-list.txt> [--names]

- Each ledger is opened `?mode=ro&immutable=1` and nothing is written.
- The bootstrap seed is fixed at 7, so output is deterministic.
- The run below used 108 ledgers: the room list beside backlog item AGEN-RALLY-NEXT-m590zk409zjqpzxdevzb3, plus `~/.claude`, `~`, and `~/dev/git-folder/*`. One ledger was skipped as corrupt.
- Repos are anonymized because this repo is public.

## Output

```
repos=22 with >=2 pickups; repos with >=20 pickups (bootstrap base, small): 4

Choosing N: below 20 pickups the nearest-rank p95 is the slowest pickup seen.
Bootstrap of that estimate against each large repo's full-history p95:
  N= 3: P(estimate < true p95)=0.79  median estimate/true=0.39  repos qualifying=15/22
  N= 5: P(estimate < true p95)=0.68  median estimate/true=0.69  repos qualifying=13/22
  N= 8: P(estimate < true p95)=0.54  median estimate/true=0.98  repos qualifying=13/22
  N=10: P(estimate < true p95)=0.46  median estimate/true=1.00  repos qualifying=11/22
  N=14: P(estimate < true p95)=0.34  median estimate/true=1.00  repos qualifying=7/22
  N=20: P(estimate < true p95)=0.56  median estimate/true=0.98  repos qualifying=4/22

Fastest qualifying repo p95 (n>=8): 19.3 h; floor 1 h never binds on a measured repo.

| Repo | pickups n | own p95 | threshold (N=8, floor 1 h) |
|---|---|---|---|
| room-01 | 212 | 193.2 h | 193 h |
| room-02 | 27 | 445.4 h | 445 h |
| room-03 | 24 | 385.5 h | 386 h |
| room-04 | 22 | 43.3 h | 43 h |
| room-05 | 19 | 220.2 h | 220 h |
| room-06 | 16 | 19.3 h | 19 h |
| room-07 | 14 | 66.3 h | 66 h |
| room-08 | 12 | 223.8 h | 224 h |
| room-09 | 12 | 23.7 h | 24 h |
| room-10 | 11 | 109.0 h | 109 h |
| room-11 | 10 | 32.1 h | 32 h |
| room-12 | 9 | 92.1 h | 92 h |
| room-13 | 9 | 51.2 h | 51 h |
| room-14 | 3 | 325.9 h | off (insufficient_history) |
| room-15 | 3 | 134.2 h | off (insufficient_history) |
| room-16 | 2 | 3270.2 h | off (insufficient_history) |
| room-17 | 2 | 326.6 h | off (insufficient_history) |
| room-18 | 2 | 0.3 h | off (insufficient_history) |
| room-19 | 2 | 30.1 h | off (insufficient_history) |
| room-20 | 2 | 82.1 h | off (insufficient_history) |
| room-21 | 2 | 313.3 h | off (insufficient_history) |
| room-22 | 2 | 960.7 h | off (insufficient_history) |
```

## Choices

- **N = 8.** At n = 5 the estimate (the slowest pickup so far) reads a median 0.69× the true p95 and falls below it 68% of the time, so the age label would fire early. At n = 8 the median is 0.98× and the estimate falls below the true p95 54% of the time, which is roughly unbiased. In this data the same 13 repos qualify at N = 5 and N = 8, so the stricter N costs no coverage. ⚠️ The bootstrap rests on only 4 repos with ≥ 20 pickups; refit when more repos qualify.
- **Floor = 1 h.** A repo whose pickups are same-second acks would otherwise get a threshold of minutes or zero, and nearly every item would be labelled "revalidate". The fastest qualifying repo's p95 is 19 h, so the floor never binds on a measured repo.
