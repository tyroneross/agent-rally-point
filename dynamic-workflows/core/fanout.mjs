// SPDX-FileCopyrightText: 2025-2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
// SPDX-License-Identifier: Apache-2.0
//
// Fan-out resolver — replaces the former hardcoded "≤4 parallel" rule.
//
// WHY THIS EXISTS. The cap used to be a flat 4 written into SKILL.md prose. A
// constant cannot say why it stopped you, so every host inherited the most
// conservative machine's answer and had no way to report which limit bound it.
// This resolves a number AND names the constraint that produced it, so a host
// that is nowhere near its ceiling can say so.
//
// WHAT RALLY KNOWS AND WHAT IT DOES NOT. Rally never spawns agents, so it has
// no model, token ledger, or CPU picture — those belong to the host. The host
// passes its own resource ceiling as `hostCap`; Rally owns only the structural
// limits every host shares: the hard ceiling and the ready-task count. This is
// the same min-of-named-caps shape build-loop's scripts/parallelism.py uses,
// minus the parts that require knowing the model.
//
// SAFETY NOTE. Parallelism is NOT what makes concurrent writes safe — disjoint
// `owns` is, and workstream-lint.mjs proves that before any dispatch regardless
// of N. Raising this number does not weaken the write-boundary guarantee. What
// does scale with N is coordination overhead: each hook fire writes ledger
// lines back into the ledger, so cost grows with agent count. That, not write
// safety, is what HARD_CEILING guards.

/** Never exceed, on any host — coordination / ledger overhead. */
export const HARD_CEILING = 12;

/** Default when neither the caller nor host config supplies a number. */
export const DEFAULT_MAX = 10;

function positiveInt(value) {
  return Number.isInteger(value) && value > 0 ? value : null;
}

/**
 * Resolve the Tier-1 fan-out width and expose every constraint behind it.
 *
 *   const { effective_max, limiting_factors } = resolveFanout({ readyTasks: 7 });
 *   const run = createLimiter(effective_max);
 *
 * @param {object}  [opts]
 * @param {number}  [opts.requested]  Caller override. Wins over configMax.
 * @param {number}  [opts.configMax]  Host/workstream config. Defaults to DEFAULT_MAX.
 * @param {number}  [opts.hostCap]    Host's own resource ceiling (CPU- or token-led).
 *                                    Omit when the host has no resource picture.
 * @param {number}  [opts.readyTasks] Dispatchable tasks right now. Never spawn
 *                                    more agents than there is work for.
 * @returns {{effective_max: number, limiting_factors: string[], caps: Record<string, number>,
 *            requested: number|null, config_max: number, host_cap: number|null,
 *            ready_tasks: number|null, hard_ceiling: number}}
 */
export function resolveFanout(opts = {}) {
  const requested = positiveInt(opts.requested);
  const configMax = positiveInt(opts.configMax) ?? DEFAULT_MAX;
  const hostCap = positiveInt(opts.hostCap);
  const readyTasks = positiveInt(opts.readyTasks);

  const caps = {
    requested_or_config: requested ?? configMax,
    hard_ceiling: HARD_CEILING,
  };
  if (hostCap !== null) caps.host = hostCap;
  if (readyTasks !== null) caps.ready_tasks = readyTasks;

  const effective = Math.max(1, Math.min(...Object.values(caps)));

  return {
    effective_max: effective,
    limiting_factors: Object.keys(caps)
      .filter((name) => caps[name] === effective)
      .sort(),
    caps,
    requested,
    config_max: configMax,
    host_cap: hostCap,
    ready_tasks: readyTasks,
    hard_ceiling: HARD_CEILING,
  };
}
