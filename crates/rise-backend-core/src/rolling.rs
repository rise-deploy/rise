//! Pure helpers for the rolling-recreate throttle and the in-rotation /
//! readiness decisions of the Docker cutover. All `&self`-free so they can be
//! unit-tested without a daemon or Traefik API.

use std::collections::{HashMap, HashSet};

use crate::diff::{action_key, ActualContainer, ReconcileAction};

/// Apply the ROLLING-RECREATE throttle to a diff's actions, run AFTER
/// [`super::diff::diff_desired_vs_actual`] and BEFORE `apply_actions`. Enforces,
/// per spec (grouped by the spec key = identity minus replica), that a *running
/// but drifted* replica is recreated at most ONE-AT-A-TIME and only while every
/// OTHER replica of that spec is currently HEALTHY — so a rollout never drops
/// more than one replica of capacity at once.
///
/// Pass-through rules (PER SPEC):
/// - **Create** / **Remove** → ALWAYS pass (initial rollout / scale-up add
///   capacity; scale-down / GC remove it). Never throttled.
/// - **Recreate** of a recovery replica — the matched live container's `state`
///   is NOT "running" (crashed / exited / created / missing) → ALWAYS pass; it's
///   already down, so recreating only restores capacity.
/// - **Recreate** of a rollout replica — the matched live container IS "running"
///   but drifted → emit AT MOST ONE per spec this tick, the lowest replica index,
///   and ONLY IF every OTHER replica of the spec is HEALTHY
///   (`healthy_by_identity`). If ANY sibling is unhealthy/starting, defer ALL
///   rollout recreates for the spec this tick.
///
/// INVARIANT: for a single spec with R running+drifted+healthy replicas this
/// yields exactly ONE rollout Recreate; with any sibling unhealthy it yields
/// ZERO; recovery Recreates and all Creates/Removes pass regardless.
///
/// `actual` provides each container's run state + identity (to map an action's
/// identity → its spec + replica index + run state). `healthy_by_identity` maps
/// an `identity_key(...)` to whether that live replica passed the HTTP health
/// probe this tick (absent → treated as NOT healthy).
pub fn filter_rolling_actions(
    actions: Vec<ReconcileAction>,
    actual: &[ActualContainer],
    healthy_by_identity: &HashMap<String, bool>,
) -> Vec<ReconcileAction> {
    // identity → the live container (for run state + replica/spec grouping).
    let actual_by_identity: HashMap<String, &ActualContainer> = actual
        .iter()
        .filter_map(|a| a.identity().map(|id| (id, a)))
        .collect();

    // Collect rollout-recreate candidates per spec; pass everything else through.
    // A candidate is (replica_index, action) where the matched live container is
    // running. We then admit at most one per spec, gated on sibling health.
    let mut passed: Vec<ReconcileAction> = Vec::new();
    // spec_key → Vec<(replica, identity, action)> of running+drifted recreates.
    let mut rollout_candidates: HashMap<String, Vec<(u32, String, ReconcileAction)>> =
        HashMap::new();

    for action in actions {
        match &action {
            ReconcileAction::Create { .. } | ReconcileAction::Remove { .. } => {
                // Unthrottled: capacity add (Create) / scale-down or GC (Remove).
                passed.push(action);
            }
            ReconcileAction::Recreate { identity, .. } => {
                match actual_by_identity.get(identity) {
                    Some(a) if a.state.as_deref() == Some("running") => {
                        // Rollout candidate: a running, drifted replica. Defer the
                        // admission decision until we've seen all of them per spec.
                        let key = a.spec_identity().unwrap_or_else(|| identity.clone());
                        rollout_candidates.entry(key).or_default().push((
                            a.replica,
                            identity.clone(),
                            action,
                        ));
                    }
                    // Recovery (not running) OR no matched live container (already
                    // gone) → pass unthrottled: recreating restores capacity.
                    _ => passed.push(action),
                }
            }
        }
    }

    // Admit at most one rollout recreate per spec, the lowest replica index, and
    // only when every OTHER replica of the spec is healthy.
    for (spec, mut candidates) in rollout_candidates {
        candidates.sort_by_key(|(replica, _, _)| *replica);
        // Identities of this spec's replicas slated for a rollout recreate — they
        // don't count as "must be healthy" siblings (they're already drifted and
        // about to be replaced), so a uniform drift across all replicas still
        // rolls one-by-one rather than deadlocking.
        let candidate_identities: HashSet<String> =
            candidates.iter().map(|(_, id, _)| id.clone()).collect();

        // Gate: every OTHER replica of this spec (a live replica NOT itself a
        // rollout candidate) must be healthy. If any such sibling is
        // unhealthy/starting, defer ALL rollout recreates for this spec.
        let siblings_healthy = actual
            .iter()
            .filter(|a| a.spec_identity().as_deref() == Some(spec.as_str()))
            .filter_map(|a| a.identity())
            .filter(|id| !candidate_identities.contains(id))
            .all(|id| healthy_by_identity.get(&id).copied().unwrap_or(false));

        if siblings_healthy {
            if let Some((_, _, action)) = candidates.into_iter().next() {
                passed.push(action);
            }
        }
        // else: defer ALL of this spec's rollout recreates this tick.
    }

    passed.sort_by_key(action_key);
    passed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::diff_desired_vs_actual;
    use crate::test_helpers::*;

    #[test]
    fn rolling_throttle_running_drifted_emits_exactly_one_recreate() {
        // INVARIANT (a): 3 running + drifted (image) + healthy replicas →
        // the diff produces 3 Recreates, but the rolling throttle admits exactly
        // ONE (the lowest replica index, r0).
        let desired_all = vec![desired_replica(0), desired_replica(1), desired_replica(2)];
        let actual = vec![
            actual_replica(0, "running", "img:OLD"),
            actual_replica(1, "running", "img:OLD"),
            actual_replica(2, "running", "img:OLD"),
        ];
        let raw = diff_desired_vs_actual(&desired_all, &actual, "rise", &no_protected());
        assert_eq!(
            raw.iter()
                .filter(|a| matches!(a, ReconcileAction::Recreate { .. }))
                .count(),
            3,
            "diff alone wants to recreate all three"
        );
        let healthy = all_healthy(&actual);
        let throttled = filter_rolling_actions(raw, &actual, &healthy);
        let recreates: Vec<&ReconcileAction> = throttled
            .iter()
            .filter(|a| matches!(a, ReconcileAction::Recreate { .. }))
            .collect();
        assert_eq!(recreates.len(), 1, "rolling admits exactly one per spec");
        match recreates[0] {
            ReconcileAction::Recreate { name, .. } => {
                assert!(
                    name.contains("_r0_"),
                    "lowest replica index rolls first: {name}"
                );
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn rolling_throttle_defers_all_when_a_sibling_is_unhealthy() {
        // INVARIANT (b): mid-rollout. r0 was already recreated (now matches
        // desired, so it is NOT a candidate) but is still STARTING (unhealthy);
        // r1 and r2 are still drifted (running, img:OLD → rollout candidates).
        // Because the non-candidate sibling r0 is unhealthy, ALL rollout recreates
        // for the spec are deferred this tick → ZERO Recreates. This is the gate
        // that makes the rollout wait for the previous replica to come back up.
        let desired_all = vec![desired_replica(0), desired_replica(1), desired_replica(2)];
        let actual = vec![
            actual_replica(0, "running", "img:1"), // already updated, matches
            actual_replica(1, "running", "img:OLD"), // drifted → candidate
            actual_replica(2, "running", "img:OLD"), // drifted → candidate
        ];
        let raw = diff_desired_vs_actual(&desired_all, &actual, "rise", &no_protected());
        // r0 (the just-recreated, non-candidate sibling) is still unhealthy.
        let mut healthy = all_healthy(&actual);
        healthy.insert(actual[0].identity().unwrap(), false);
        let throttled = filter_rolling_actions(raw, &actual, &healthy);
        assert_eq!(
            throttled
                .iter()
                .filter(|a| matches!(a, ReconcileAction::Recreate { .. }))
                .count(),
            0,
            "an unhealthy (still-starting) sibling defers ALL rollout recreates"
        );
    }

    #[test]
    fn rolling_throttle_uniform_drift_rolls_one_at_a_time() {
        // When EVERY replica is drifted (all candidates), there are no
        // non-candidate siblings to gate on, so the rollout begins: exactly one
        // (r0) is recreated this tick. On the next tick r0 matches desired and
        // becomes the sibling whose health gates the rest — yielding the
        // one-at-a-time roll across ticks.
        let desired_all = vec![desired_replica(0), desired_replica(1), desired_replica(2)];
        let actual = vec![
            actual_replica(0, "running", "img:OLD"),
            actual_replica(1, "running", "img:OLD"),
            actual_replica(2, "running", "img:OLD"),
        ];
        let raw = diff_desired_vs_actual(&desired_all, &actual, "rise", &no_protected());
        let healthy = all_healthy(&actual);
        let throttled = filter_rolling_actions(raw, &actual, &healthy);
        assert_eq!(
            throttled
                .iter()
                .filter(|a| matches!(a, ReconcileAction::Recreate { .. }))
                .count(),
            1,
            "uniform drift still rolls one replica at a time"
        );
    }

    #[test]
    fn rolling_throttle_recovery_recreate_passes_even_with_unhealthy_sibling() {
        // INVARIANT (c): a CRASHED (state != running) replica's Recreate is a
        // recovery — it passes UNTHROTTLED even though a sibling is unhealthy.
        // Here r0 has exited; r1 is running-drifted-unhealthy. Only r0's recovery
        // recreate is admitted (r1's rollout recreate is gated out).
        let desired_all = vec![desired_replica(0), desired_replica(1)];
        let actual = vec![
            actual_replica(0, "exited", "img:1"),    // crashed → recovery
            actual_replica(1, "running", "img:OLD"), // running-drifted → rollout
        ];
        let raw = diff_desired_vs_actual(&desired_all, &actual, "rise", &no_protected());
        // The crashed r0 is (realistically) unhealthy; it is r1's only sibling, so
        // r1's rollout recreate is GATED OUT. r0's recovery recreate still passes.
        let mut healthy = all_healthy(&actual);
        healthy.insert(actual[0].identity().unwrap(), false);
        let throttled = filter_rolling_actions(raw, &actual, &healthy);
        let recreates: Vec<&ReconcileAction> = throttled
            .iter()
            .filter(|a| matches!(a, ReconcileAction::Recreate { .. }))
            .collect();
        assert_eq!(recreates.len(), 1, "only the recovery recreate passes");
        match recreates[0] {
            ReconcileAction::Recreate { name, .. } => {
                assert!(
                    name.contains("_r0_"),
                    "recovery (crashed) replica passes: {name}"
                );
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn rolling_throttle_creates_and_removes_always_pass() {
        // INVARIANT (d): Create (scale-up) and Remove (scale-down/GC) actions are
        // never throttled — capacity changes apply immediately. Mix a Create
        // (r2 missing), a Remove (surplus r3), and a running-drifted r0 (rollout)
        // with r1 healthy. The Create + Remove pass; exactly one rollout recreate
        // passes too (r0, siblings healthy).
        let desired_all = vec![desired_replica(0), desired_replica(1), desired_replica(2)];
        let actual = vec![
            actual_replica(0, "running", "img:OLD"), // rollout recreate
            actual_replica(1, "running", "img:1"),   // matches → no action
            actual_replica(3, "running", "img:1"),   // surplus → Remove
        ];
        let raw = diff_desired_vs_actual(&desired_all, &actual, "rise", &no_protected());
        let healthy = all_healthy(&actual);
        let throttled = filter_rolling_actions(raw, &actual, &healthy);
        assert!(
            throttled.iter().any(
                |a| matches!(a, ReconcileAction::Create { name, .. } if name.contains("_r2_"))
            ),
            "scale-up Create passes"
        );
        assert!(
            throttled
                .iter()
                .any(|a| matches!(a, ReconcileAction::Remove { id, .. } if id == "cid-r3")),
            "scale-down Remove passes"
        );
        assert_eq!(
            throttled
                .iter()
                .filter(|a| matches!(a, ReconcileAction::Recreate { .. }))
                .count(),
            1,
            "exactly one rollout recreate (r0) admitted"
        );
    }
}
