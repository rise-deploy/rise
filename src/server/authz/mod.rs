//! The centralized authorization choke point for the generic resource API
//! (ADR-0001 §4, §5; Implementation structure, Tier 2).
//!
//! Every request to `/api/v1/resources` runs through here, and every decision is
//! the same three steps:
//!
//! 1. Resolve the credential to one typed [`AuthenticatedPrincipal`] and build a
//!    request-local [`AuthorizationSnapshot`] from live facts.
//! 2. Evaluate `(verb, ResourceKind, subresource?)` against the resource's own
//!    ancestry and effective labels — per item, never scope-wide.
//! 3. For a write that changes authorization facts, run the grant gate over the
//!    same transaction that performs the mutation.
//!
//! Nothing here decides policy. The decisions live in `rise-authz`; this module
//! supplies the facts, owns the transaction, and turns a refusal into an HTTP
//! response.
//!
//! **What replaced `require_operator`.** Operator standing is no longer a gate
//! in front of the API: it is one subject in the model, whose seeded
//! `system-admin` binding grants every verb and whose requests ignore every Deny
//! (§1). An operator therefore sees exactly what they saw before. What changed is
//! that everyone else is now evaluated rather than refused, so an ordinary
//! principal reaches precisely what stored policy grants them.

mod change;
mod membership;
mod projection;

use std::sync::{Arc, Mutex};

use rise_authz::engine::{
    AuthenticatedPrincipal, AuthorizationCap, AuthorizationEngine, AuthorizationError,
    AuthorizationSnapshot, GateOutcome, GateRejection, ListCandidate, ListDecision,
    RejectionReason, ResourceNode, ResourceTree,
};
use rise_authz::policy::{Decision, PermissionTuple};
use rise_resource_api::{
    ResourceKind, ResourceRow, ResourceStore, SubjectId, SubresourceName, Verb,
};
use rise_resource_store_postgres::{PgResourceStore, PgSession, SerializableTransaction};
use sqlx::PgPool;
use uuid::Uuid;

use crate::db::models::User;
use crate::server::auth::context::AuthContext;
use crate::server::error::ServerError;
use crate::server::resources::error_map::{store_error_to_server_error, RESOURCE_NOT_FOUND};

pub use change::{
    change_for_create, change_for_delete, change_for_scheduled_deletion, change_for_update,
    label_changes, AuthorizationChangeSet,
};
pub use membership::{OperatorSelectors, RiseMembershipResolver};
pub use projection::{project_list_item, ReadGranularity};

/// How many times a serializable write is replayed before the caller is told to
/// retry.
///
/// ADR-0001 §5 requires the retry to be bounded, not unbounded: a write that
/// keeps losing is contending with something, and turning that into an
/// unbounded loop hides the contention behind latency. Three attempts absorb the
/// ordinary case — one concurrent policy write — while a persistently losing
/// request surfaces as a 503 the caller can see.
const MAX_SERIALIZATION_ATTEMPTS: u32 = 3;

/// Builds authorization contexts for requests, over the pool or over one
/// serializable transaction.
#[derive(Clone)]
pub struct ResourceAuthorizer {
    store: Arc<PgResourceStore>,
    pool: PgPool,
    operators: OperatorSelectors,
}

impl ResourceAuthorizer {
    pub fn new(store: Arc<PgResourceStore>, pool: PgPool, operators: OperatorSelectors) -> Self {
        Self {
            store,
            pool,
            operators,
        }
    }

    /// The principal behind a credential.
    ///
    /// Only a Rise-authenticated User reaches the generic resource API today.
    /// Workload principals authenticate through the exchange endpoint and are
    /// bound to a Project, which is a typed concept the generic API does not
    /// share; ADR-0001 §7's target-bound `/token` route is what makes them
    /// principals here, and it is not built yet.
    ///
    /// The subject is the User's stable identity as this install expresses it:
    /// the typed `users` row's UID, which is also the credential's `rise_uid`.
    /// ADR-0001 §1's generated `User` resource name replaces it when identity
    /// resources go live; both are opaque, immutable, and never the email.
    fn principal(auth: &AuthContext) -> Result<(AuthenticatedPrincipal, User), ServerError> {
        let user = auth.user()?.clone();
        let subject: SubjectId = format!("user:{}", user.id).parse().map_err(|error| {
            ServerError::internal(format!("principal is not a valid subject: {error}"))
        })?;
        let principal =
            // Unrestricted: authorization details (§7) are not issued yet, so no
            // credential carries a ceiling. When they are, the parsed cap
            // arrives here and every decision below intersects with it, with no
            // other change.
            AuthenticatedPrincipal::new(subject, user.id, AuthorizationCap::Unrestricted)
                .map_err(authorization_error_to_server_error)?;
        Ok((principal, user))
    }

    /// A read-path context. Facts are read through the pool at the default
    /// isolation: a read decides against current facts and commits nothing.
    pub async fn read_context(
        &self,
        auth: &AuthContext,
    ) -> Result<AuthorizationContext, ServerError> {
        let (principal, user) = Self::principal(auth)?;
        self.context(principal, user, self.store.session()).await
    }

    async fn context(
        &self,
        principal: AuthenticatedPrincipal,
        user: User,
        session: PgSession,
    ) -> Result<AuthorizationContext, ServerError> {
        let store: Arc<dyn ResourceStore> = Arc::new(self.store.in_session(session.clone()));
        let engine = AuthorizationEngine::new(
            store.clone(),
            Arc::new(RiseMembershipResolver::new(
                session.clone(),
                store.clone(),
                self.operators.clone(),
                user.clone(),
            )),
        );
        let snapshot = engine
            .snapshot(principal)
            .await
            .map_err(authorization_error_to_server_error)?;
        Ok(AuthorizationContext {
            engine,
            snapshot,
            store,
            session,
            actor: user.email,
            attempt: 1,
            deferred_audit: Mutex::new(Vec::new()),
        })
    }

    /// Open one `SERIALIZABLE` unit of work for an authorization-changing
    /// request (ADR-0001 §5).
    ///
    /// Everything the caller then does — the ordinary authority check, the grant
    /// gate, and the mutation — happens on one connection inside one
    /// transaction, so the facts the gate compared are the facts the write
    /// commits against. A concurrent revocation either precedes the check or
    /// forces the attempt to be replayed.
    ///
    /// `attempt` is the 1-based replay counter, carried only so the audit
    /// record can say which try it is: gate comparisons and refusals are logged
    /// inline (a comparison happened, whatever the transaction goes on to do),
    /// so a replayed write emits the line more than once and an unmarked
    /// duplicate would read as two separate attempts to delegate.
    pub async fn begin_write(
        &self,
        auth: &AuthContext,
        attempt: u32,
    ) -> Result<WriteAttempt, ServerError> {
        // Resolved before the transaction opens: a credential this API does not
        // accept should not cost one, and on a retry loop it would cost one per
        // attempt.
        let (principal, user) = Self::principal(auth)?;
        let transaction = SerializableTransaction::begin(&self.pool)
            .await
            .map_err(store_error_to_server_error)?;
        let mut context = self.context(principal, user, transaction.session()).await?;
        context.attempt = attempt;
        Ok(WriteAttempt {
            transaction,
            context,
        })
    }

    /// Whether a failed attempt should be replayed rather than reported.
    ///
    /// The retry is bounded, not unbounded: a write that keeps losing is
    /// contending with something, and looping forever would hide that behind
    /// latency instead of surfacing it as a 503 the caller can act on.
    pub fn should_retry(error: &ServerError, attempt: u32) -> bool {
        let retry = error.retryable && attempt < MAX_SERIALIZATION_ATTEMPTS;
        if retry {
            tracing::info!(
                attempt,
                "Authorization-changing write lost a serialization race; retrying"
            );
        }
        retry
    }
}

/// One attempt at a serializable write: the transaction, and the authorization
/// context reading and writing through it.
///
/// Dropping without [`Self::commit`] rolls the attempt back, so an early return
/// on a refused write leaves nothing behind.
pub struct WriteAttempt {
    transaction: SerializableTransaction,
    context: AuthorizationContext,
}

impl WriteAttempt {
    pub fn context(&self) -> &AuthorizationContext {
        &self.context
    }

    /// Commit, then emit the audit records the attempt deferred.
    ///
    /// A serialization failure here is the ordinary case — PostgreSQL detects
    /// most conflicts at commit rather than at the statement — and arrives as a
    /// retryable error, in which case the deferred records are dropped along
    /// with the writes they describe.
    pub async fn commit(self) -> Result<(), ServerError> {
        self.transaction
            .commit()
            .await
            .map_err(store_error_to_server_error)?;
        self.context.flush_audit();
        Ok(())
    }
}

/// One request's authorization facts, plus the store they were read through.
///
/// On a write path the store is transaction-scoped, so `store()` is also how the
/// caller performs the mutation: reading the facts and writing the row through
/// the same handle is what makes the check and the mutation one operation.
pub struct AuthorizationContext {
    engine: AuthorizationEngine,
    snapshot: AuthorizationSnapshot,
    store: Arc<dyn ResourceStore>,
    session: PgSession,
    actor: String,
    /// Which replay of the write this context belongs to, 1-based. Read-path
    /// contexts are always 1.
    attempt: u32,
    /// Audit records for writes this attempt made, held until it commits.
    ///
    /// Emitting them inline would claim writes that a serialization retry then
    /// rolled back — and a record of a create that never happened is worse than
    /// no record, because the whole point of the trail is that it is true.
    /// Decisions are different and stay inline: a refusal and a gate comparison
    /// both happened, whatever the transaction goes on to do.
    deferred_audit: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
}

impl AuthorizationContext {
    /// The store this request's facts were read through — the transaction's, on
    /// a write path.
    pub fn store(&self) -> &Arc<dyn ResourceStore> {
        &self.store
    }

    /// This request's own session, for the typed-table reads a resource write
    /// still depends on — the Organization delete guard's child count, most of
    /// all. Running them here rather than on the pool is what puts the guard and
    /// the delete in one transaction.
    ///
    /// A transaction-scoped session lends out one connection at a time, so a
    /// caller must release it before calling back into the store.
    pub fn session(&self) -> &PgSession {
        &self.session
    }

    /// The caller's email, for audit records. Not an identity: policy never
    /// matches on it (ADR-0001 §1).
    pub fn actor(&self) -> &str {
        &self.actor
    }

    pub fn subject(&self) -> &SubjectId {
        self.snapshot.principal().subject()
    }

    /// Record a completed write, to be emitted once the transaction commits.
    ///
    /// The closure owns everything it prints, so nothing borrowed from the
    /// attempt outlives it.
    pub fn audit(&self, record: impl FnOnce() + Send + 'static) {
        self.deferred_audit
            .lock()
            .expect("deferred audit records")
            .push(Box::new(record));
    }

    /// Emit every deferred record. Called by [`WriteAttempt::commit`].
    fn flush_audit(&self) {
        let records =
            std::mem::take(&mut *self.deferred_audit.lock().expect("deferred audit records"));
        for record in records {
            record();
        }
    }

    pub fn is_operator(&self) -> bool {
        self.snapshot.membership().is_operator
    }

    /// A stored resource's evaluation target: its root-first ancestry.
    pub async fn tree(&self, uid: Uuid) -> Result<ResourceTree, ServerError> {
        self.engine
            .resource_tree(uid)
            .await
            .map_err(authorization_error_to_server_error)
    }

    /// Whether the caller may perform one tuple on one target.
    pub async fn allows(
        &self,
        target: &ResourceTree,
        verb: Verb,
        subresource: Option<&SubresourceName>,
    ) -> Result<bool, ServerError> {
        let tuple = PermissionTuple {
            verb,
            kind: target.leaf().kind.clone(),
            subresource: subresource.cloned(),
        };
        Ok(self
            .engine
            .authorize(&self.snapshot, target, &tuple)
            .await
            .map_err(authorization_error_to_server_error)?
            == Decision::Allow)
    }

    /// Require one tuple, or refuse the request.
    ///
    /// A refusal is a 403 naming the verb and kind. It deliberately does not
    /// name the bindings that were or were not found: `Explanation` reports
    /// Roles and binding UIDs the caller may hold no read access to (ADR-0001
    /// §5, "Restrictions remain transparent" — for a reader with access to them,
    /// not for the refused caller).
    pub async fn require(
        &self,
        target: &ResourceTree,
        verb: Verb,
        subresource: Option<&SubresourceName>,
    ) -> Result<(), ServerError> {
        if self.allows(target, verb, subresource).await? {
            return Ok(());
        }
        let leaf = target.leaf();
        let what = match subresource {
            None => format!("{}", leaf.kind),
            Some(name) => format!("{} {name}", leaf.kind),
        };
        tracing::warn!(
            target: "rise::audit",
            actor = %self.actor,
            subject = %self.subject(),
            verb = %verb_name(verb),
            kind = %leaf.kind,
            name = %leaf.name,
            subresource = subresource.map(|s| s.as_ref()),
            "resource.access_denied"
        );
        Err(ServerError::forbidden(format!(
            "not authorized to {} {what} '{}'",
            verb_name(verb),
            leaf.name
        )))
    }

    /// Require one tuple on a resource the caller addressed *by name*, masking
    /// the resource's existence from a caller who may not read it.
    ///
    /// The collection path answers a listing the caller has no grant in with an
    /// empty list rather than a 403, so that the ancestor tree is not
    /// enumerable by name (ADR-0001 §4). A 403 on an item addressed by name
    /// undoes exactly that: probe `…/organizations/acme`, read the status code,
    /// and the masked listing above it has told you nothing the item path will
    /// not confirm one name at a time.
    ///
    /// So the answer turns on `get`, not on the verb being attempted. A caller
    /// who may read the resource gets an ordinary 403 naming the verb they are
    /// short — they already know it exists, and a refusal they cannot read is
    /// just a worse error message. A caller who may not read it is told the
    /// same thing a nonexistent name is told. Both are audited as denials; only
    /// the wording differs.
    pub async fn require_visible(
        &self,
        target: &ResourceTree,
        verb: Verb,
        subresource: Option<&SubresourceName>,
    ) -> Result<(), ServerError> {
        if self.allows(target, verb, subresource).await? {
            return Ok(());
        }
        // The fallback is keyed on the main-resource read, so `(get, Kind, sub)`
        // — a different tuple from `(get, Kind)` — still reaches it. Comparing
        // the verb alone would mask a subresource refusal from a caller holding
        // full `get` on the resource, which discloses nothing and is simply a
        // worse answer.
        if (verb, subresource) != (Verb::Get, None) && self.allows(target, Verb::Get, None).await? {
            return self.require(target, verb, subresource).await;
        }
        let leaf = target.leaf();
        tracing::warn!(
            target: "rise::audit",
            actor = %self.actor,
            subject = %self.subject(),
            verb = %verb_name(verb),
            kind = %leaf.kind,
            name = %leaf.name,
            subresource = subresource.map(|s| s.as_ref()),
            "resource.access_denied_masked"
        );
        Err(ServerError::not_found(RESOURCE_NOT_FOUND))
    }

    /// Require `create`, masking a parent the caller may not read.
    ///
    /// The leaf does not exist yet, so there is nothing about *it* to conceal —
    /// but the answer is still a fact about its parent. The list path under an
    /// ancestor that does not exist answers empty rather than 404 precisely so
    /// the tree is not enumerable, and a create that answers 403 for a real
    /// parent and 404 for an absent one puts that back. So a refused create
    /// under a parent the caller cannot read reads as "no such path"; one under
    /// a parent they *can* read names the verb they are short.
    ///
    /// `parent` is `None` for a root-scoped kind, where there is no parent to
    /// conceal and the ordinary refusal applies.
    pub async fn require_create(
        &self,
        target: &ResourceTree,
        parent: Option<&ResourceTree>,
    ) -> Result<(), ServerError> {
        if self.allows(target, Verb::Create, None).await? {
            return Ok(());
        }
        if let Some(parent) = parent {
            if !self.allows(parent, Verb::Get, None).await? {
                let leaf = parent.leaf();
                tracing::warn!(
                    target: "rise::audit",
                    actor = %self.actor,
                    subject = %self.subject(),
                    verb = "create",
                    kind = %leaf.kind,
                    name = %leaf.name,
                    "resource.access_denied_masked"
                );
                return Err(ServerError::not_found(RESOURCE_NOT_FOUND));
            }
        }
        self.require(target, Verb::Create, None).await
    }

    /// Per-item `list`/`get` granularity for a collection (ADR-0001 §4).
    pub async fn filter_list(
        &self,
        ancestors: &[ResourceNode],
        candidates: &[ListCandidate],
    ) -> Result<Vec<ListDecision>, ServerError> {
        self.engine
            .filter_list(&self.snapshot, ancestors, candidates)
            .await
            .map_err(authorization_error_to_server_error)
    }

    /// Run ADR-0001 §5's grant gate over one authorization-changing change.
    ///
    /// Called after ordinary write authority has been established and before the
    /// mutation, inside the same transaction as both.
    pub async fn gate(
        &self,
        operation: &str,
        detail: Option<&str>,
        change: &rise_authz::engine::AuthorizationChange,
        disclosure: Disclosure,
    ) -> Result<(), ServerError> {
        let outcome = self
            .engine
            .evaluate_grant(&self.snapshot, change)
            .await
            .map_err(authorization_error_to_server_error)?;
        self.record_gate(operation, detail, &outcome);
        if outcome.permitted() {
            return Ok(());
        }
        Err(gate_rejection_to_server_error(
            operation, &outcome, disclosure,
        ))
    }

    /// Audit what the gate compared, including the operator short-circuit — an
    /// operator produces no claims, so the record is the only evidence the write
    /// was gated at all.
    ///
    /// A rejection's recipients and domains are recorded here in full. They are
    /// facts about *stored* policy and topology, so this is where they belong:
    /// legible to a reader with access to the audit stream, and not echoed back
    /// to a caller who may hold no read access to any of it.
    fn record_gate(&self, operation: &str, detail: Option<&str>, outcome: &GateOutcome) {
        tracing::info!(
            target: "rise::audit",
            actor = %self.actor,
            subject = %self.subject(),
            operation,
            detail,
            operator = self.is_operator(),
            attempt = self.attempt,
            claims = outcome.claims.len(),
            rejected = outcome.rejections.len(),
            creation_exception = outcome.creation_exception,
            rejections = ?outcome.rejections,
            "resource.grant_gate"
        );
    }
}

/// What of a gate refusal may be echoed back to the caller.
///
/// A rejection carries a recipient and a policy domain, and where those come
/// from decides whether the caller may see them. When they are reconstructed
/// from the request — the subject and scope of a binding the caller just
/// wrote — echoing them tells the caller nothing they did not send. When they
/// are read out of *stored* policy or topology, they are not the caller's to
/// see: a Role edit's recipients are the subjects of every binding referencing
/// it, across organizations, and a label write's domains are the full paths of
/// every descendant inheriting the key. Rendering those would turn a refusal
/// into the enumeration oracle ADR-0001 §4 masks collections to prevent.
///
/// Suppressed detail is not lost; it goes to the `rise::audit` record.
#[derive(Debug, Clone, Copy)]
pub struct Disclosure {
    /// The recipient is a subject the caller named in this request.
    pub recipient: bool,
    /// The domain is one the caller supplied, rather than one derived from
    /// stored policy or the resource tree.
    pub domain: bool,
}

impl Disclosure {
    /// Everything in the rejection came from the request body.
    pub const FROM_REQUEST: Self = Self {
        recipient: true,
        domain: true,
    };
    /// The recipient is the caller's own; nothing else is. A Group membership
    /// write, whose domains come from every stored binding that names the
    /// Group.
    pub const RECIPIENT_ONLY: Self = Self {
        recipient: true,
        domain: false,
    };
    /// Everything is read out of stored policy.
    pub const NONE: Self = Self {
        recipient: false,
        domain: false,
    };
}

fn verb_name(verb: Verb) -> &'static str {
    match verb {
        Verb::Get => "get",
        Verb::List => "list",
        Verb::Create => "create",
        Verb::Update => "update",
        Verb::Delete => "delete",
        Verb::Use => "use",
    }
}

/// Render a gate refusal in the caller's terms.
///
/// The gate answers in policy algebra: a recipient, a domain, and the tuples the
/// writer cannot justify. A caller needs to know what they were trying to
/// delegate, so the message names the operation and the missing authority, adds
/// the recipient and domain only where [`Disclosure`] says they came from the
/// request, and translates the algebra's synthetic probe kinds — which stand for
/// "every other kind" — rather than echoing an invented resource kind.
///
/// The list of rejections is capped. One refusal can produce one claim per
/// inheriting descendant, and a caller does not need the count to learn the
/// answer; the whole set is in the audit record.
fn gate_rejection_to_server_error(
    operation: &str,
    outcome: &GateOutcome,
    disclosure: Disclosure,
) -> ServerError {
    const MAX_REJECTIONS: usize = 3;
    let mut details: Vec<String> = outcome
        .rejections
        .iter()
        .map(|rejection| render_rejection(rejection, disclosure))
        .collect();
    details.sort();
    details.dedup();
    let elided = details.len().saturating_sub(MAX_REJECTIONS);
    details.truncate(MAX_REJECTIONS);
    let more = if elided > 0 {
        format!(" (and {elided} more)")
    } else {
        String::new()
    };
    ServerError::forbidden(format!(
        "{operation} would grant authority you do not hold: {}{more}",
        details.join("; ")
    ))
}

fn render_rejection(rejection: &GateRejection, disclosure: Disclosure) -> String {
    let recipient = match (disclosure.recipient, &rejection.recipient) {
        (true, rise_resource_api::BindingSubject::Literal(subject)) => subject.to_string(),
        (true, template) => format!("subjects matching {template}"),
        // Named vaguely on purpose: the recipient is a stored binding's subject,
        // which can be another organization's Group or ServiceAccount.
        (false, _) => "another subject".to_owned(),
    };
    match &rejection.reason {
        RejectionReason::OperatorStandingRequired => {
            format!("{recipient} would gain operator standing, which only an operator may confer")
        }
        RejectionReason::InsufficientAuthority(_missing) => {
            let scope = rejection
                .domain
                .as_ref()
                .filter(|_| disclosure.domain)
                .map(|domain| {
                    let selector = match &domain.domain.selector {
                        None => String::new(),
                        Some(selector) => match &selector.value {
                            None => format!(" carrying '{}'", selector.key),
                            Some(value) => format!(" labelled '{}: {value}'", selector.key),
                        },
                    };
                    format!(" over {}{selector}", domain.domain.scope)
                })
                .unwrap_or_default();
            // The witness list is never rendered, whatever the change shape.
            // It is drawn from the claim's after-side, and that side is not the
            // caller's submission but `aggregate(...)` — the recipient's *whole*
            // effective policy over the domain, with the written change overlaid
            // onto the bindings it touches. So a witness can come from any other
            // binding delivering policy to the same recipient: removing a Deny
            // from a Role you may edit but not read names the kinds that a
            // different, unrelated Role was Allowing all along. Rendering that
            // is a read of stored policy through a refusal, and paired with the
            // writes that succeed it profiles the install. The full comparison
            // is in the `resource.grant_gate` audit record.
            format!("{recipient} would gain authority{scope} that you do not hold there")
        }
    }
}

/// Map an engine failure onto the response it deserves.
///
/// A store error keeps its own mapping so a serialization failure stays
/// retryable all the way up to the write loop. Everything else is either the
/// caller's malformed input or corrupt stored policy — and corrupt policy fails
/// closed, loudly, rather than being evaluated around.
pub fn authorization_error_to_server_error(error: AuthorizationError) -> ServerError {
    match error {
        AuthorizationError::Store(store) => store_error_to_server_error(store),
        AuthorizationError::InvalidInput(message) => ServerError::bad_request(message),
        AuthorizationError::InvalidPrincipal(message) => ServerError::unauthorized(message),
        AuthorizationError::UnknownResource(uid) => {
            ServerError::not_found(format!("resource '{uid}' not found"))
        }
        AuthorizationError::CorruptPolicy(message) => ServerError::internal_anyhow(
            anyhow::anyhow!(message),
            "stored authorization policy is invalid",
        ),
        AuthorizationError::Membership(message) => ServerError::internal_anyhow(
            anyhow::anyhow!(message),
            "failed to resolve the caller's membership",
        ),
    }
}

/// A resource's node in the tree, from its stored row.
pub fn node_for(row: &ResourceRow) -> Result<ResourceNode, ServerError> {
    Ok(ResourceNode {
        kind: row.resource_kind().map_err(|error| {
            ServerError::internal(format!("stored row has no valid kind: {error}"))
        })?,
        name: row.name.clone(),
        labels: row.labels.clone(),
    })
}

/// A proposed node for a resource being created.
pub fn node_for_new(
    api_version: &str,
    kind: &str,
    name: &str,
    labels: &std::collections::BTreeMap<String, String>,
) -> Result<ResourceNode, ServerError> {
    let group = api_version.split('/').next().unwrap_or(api_version);
    let kind = ResourceKind::new(group, kind)
        .map_err(|error| ServerError::bad_request(format!("invalid resource kind: {error}")))?;
    Ok(ResourceNode {
        kind,
        name: name.to_owned(),
        labels: labels.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rise_authz::policy::PermissionTuple;
    use rise_resource_api::BindingSubject;

    fn tuple(verb: Verb, kind: &str, subresource: Option<&str>) -> PermissionTuple {
        PermissionTuple {
            verb,
            kind: kind.parse().expect("kind"),
            subresource: subresource.map(|name| name.parse().expect("subresource")),
        }
    }

    /// A refusal names the operation, the recipient, the domain, and what is
    /// missing — not a `(scope, selector)` pair and a tuple list.
    #[test]
    fn a_rejection_names_the_recipient_and_the_domain() {
        let outcome = GateOutcome {
            claims: Vec::new(),
            rejections: vec![GateRejection {
                recipient: BindingSubject::Literal("group:acme/platform".parse().expect("subject")),
                domain: Some(rise_authz::engine::GateDomain {
                    domain: rise_authz::policy::PolicyDomain {
                        scope: "rise.dev/Organization/acme".parse().expect("scope"),
                        selector: None,
                    },
                    organization_clamped: false,
                }),
                reason: RejectionReason::InsufficientAuthority(vec![tuple(
                    Verb::Delete,
                    "example.dev/Widget",
                    None,
                )]),
            }],
            creation_exception: false,
        };
        let error = gate_rejection_to_server_error(
            "creating RoleBinding 'x'",
            &outcome,
            Disclosure::FROM_REQUEST,
        );
        assert_eq!(error.status, axum::http::StatusCode::FORBIDDEN);
        assert!(error
            .message
            .starts_with("creating RoleBinding 'x' would grant"));
        assert!(error.message.contains("group:acme/platform"));
        assert!(error.message.contains("rise.dev/Organization/acme"));
        // The subject and the scope came from the request, so both are named.
        // What the subject would have *gained* did not — the witness list is
        // drawn from their whole effective policy over that scope — so it is
        // withheld even here.
        assert!(!error.message.contains("example.dev"), "{}", error.message);
    }

    /// A rejection whose recipient and domain were read out of stored policy —
    /// a Role edit, a binding edit — says neither. Rendering them would let a
    /// refused write enumerate other organizations' subjects and the resource
    /// tree beneath the target.
    #[test]
    fn a_stored_policy_rejection_discloses_neither_recipient_nor_domain() {
        let outcome = GateOutcome {
            claims: Vec::new(),
            rejections: vec![GateRejection {
                recipient: BindingSubject::Literal(
                    "group:other-org/secret-team".parse().expect("subject"),
                ),
                domain: Some(rise_authz::engine::GateDomain {
                    domain: rise_authz::policy::PolicyDomain {
                        scope: "rise.dev/Project/other-org/project-orion"
                            .parse()
                            .expect("scope"),
                        selector: None,
                    },
                    organization_clamped: false,
                }),
                reason: RejectionReason::InsufficientAuthority(vec![tuple(
                    Verb::Delete,
                    "example.dev/Widget",
                    None,
                )]),
            }],
            creation_exception: false,
        };
        let error = gate_rejection_to_server_error(
            "editing PlatformRole 'shared'",
            &outcome,
            Disclosure::NONE,
        );
        assert!(!error.message.contains("other-org"), "{}", error.message);
        assert!(
            !error.message.contains("project-orion"),
            "{}",
            error.message
        );
        assert!(!error.message.contains("secret-team"), "{}", error.message);
        assert!(
            error.message.contains("another subject"),
            "{}",
            error.message
        );
        // Nor the witness tuples. They are drawn from the recipient's whole
        // effective policy over the domain, not from the caller's submission,
        // so any one of them can have come from a binding the caller has never
        // seen.
        assert!(!error.message.contains("example.dev"), "{}", error.message);
        assert!(!error.message.contains("delete on"), "{}", error.message);
    }

    /// A label write discloses neither side. The gate resolves the recipient
    /// out of a *stored* binding — always a `Literal`, never anything echoed
    /// from the request — and the domains are the written resource and every
    /// descendant inheriting the key.
    #[test]
    fn a_label_rejection_discloses_neither_recipient_nor_subtree() {
        let outcome = GateOutcome {
            claims: Vec::new(),
            rejections: vec![GateRejection {
                recipient: BindingSubject::Literal(
                    "group:acme/platform-admins".parse().expect("subject"),
                ),
                domain: Some(rise_authz::engine::GateDomain {
                    domain: rise_authz::policy::PolicyDomain {
                        scope: "rise.dev/Environment/acme/app/env-prod"
                            .parse()
                            .expect("scope"),
                        selector: None,
                    },
                    organization_clamped: false,
                }),
                reason: RejectionReason::InsufficientAuthority(vec![tuple(
                    Verb::Delete,
                    "rise.dev/Environment",
                    None,
                )]),
            }],
            creation_exception: false,
        };
        let error = gate_rejection_to_server_error(
            "setting label 'rise.dev/owner' to 'user:x'",
            &outcome,
            change::label_disclosure(),
        );
        assert!(!error.message.contains("env-prod"), "{}", error.message);
        assert!(
            !error.message.contains("platform-admins"),
            "{}",
            error.message
        );
        assert!(
            error.message.contains("another subject"),
            "{}",
            error.message
        );
    }

    /// Only an operator may confer operator standing, and the refusal says so
    /// rather than listing tuples nobody can satisfy.
    #[test]
    fn an_operator_standing_refusal_says_what_it_is() {
        let outcome = GateOutcome {
            claims: Vec::new(),
            rejections: vec![GateRejection {
                recipient: BindingSubject::Literal("user:u-1".parse().expect("subject")),
                domain: None,
                reason: RejectionReason::OperatorStandingRequired,
            }],
            creation_exception: false,
        };
        let error = gate_rejection_to_server_error(
            "mapping an identity",
            &outcome,
            Disclosure::FROM_REQUEST,
        );
        assert!(
            error.message.contains("only an operator may confer"),
            "{}",
            error.message
        );
    }
}
