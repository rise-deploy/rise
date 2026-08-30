-- The controller-observed pod snapshot is no longer produced (ADR-0006 D1):
-- a deployment's observability is its event log. Existing rows still carry the
-- last snapshot each controller wrote, and `controller_metadata` is surfaced
-- verbatim for introspection, so leaving them would display state that stopped
-- advancing at upgrade time.
--
-- Only the two retired keys are removed. `controller_metadata` remains the
-- controllers' own bookkeeping: the ECS reconciler keeps its converged
-- task-definition hash under `ecs`, and dropping that would make every ECS
-- service read as drifted and roll once on the next reconcile.
UPDATE deployments
SET controller_metadata = controller_metadata - 'pod_status' - 'health'
WHERE controller_metadata ?| ARRAY['pod_status', 'health'];
