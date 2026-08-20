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
use crate::server::resources::error_map::store_error_to_server_error;

pub use change::{
    change_for_create, change_for_delete, change_for_update, label_changes, AuthorizationChangeSet,
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
        self.context(auth, self.store.session()).await
    }

    async fn context(
        &self,
        auth: &AuthContext,
        session: PgSession,
    ) -> Result<AuthorizationContext, ServerError> {
        let (principal, user) = Self::principal(auth)?;
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
    pub async fn begin_write(&self, auth: &AuthContext) -> Result<WriteAttempt, ServerError> {
        let transaction = SerializableTransaction::begin(&self.pool)
            .await
            .map_err(store_error_to_server_error)?;
        let context = self.context(auth, transaction.session()).await?;
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

    /// The evaluation target for a resource that has no row yet: the parent's
    /// ancestry plus the proposed leaf (ADR-0001 §4 — a create is authorized
    /// against the resource it would produce, labels included).
    pub async fn tree_for_new(
        &self,
        parent_uid: Option<Uuid>,
        leaf: ResourceNode,
    ) -> Result<ResourceTree, ServerError> {
        let ancestors = match parent_uid {
            None => Vec::new(),
            Some(parent) => self.tree(parent).await?.nodes().to_vec(),
        };
        Ok(ResourceTree::with_leaf(&ancestors, leaf))
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
        change: &rise_authz::engine::AuthorizationChange,
    ) -> Result<(), ServerError> {
        let outcome = self
            .engine
            .evaluate_grant(&self.snapshot, change)
            .await
            .map_err(authorization_error_to_server_error)?;
        self.record_gate(operation, &outcome);
        if outcome.permitted() {
            return Ok(());
        }
        Err(gate_rejection_to_server_error(operation, &outcome))
    }

    /// Audit what the gate compared, including the operator short-circuit — an
    /// operator produces no claims, so the record is the only evidence the write
    /// was gated at all.
    fn record_gate(&self, operation: &str, outcome: &GateOutcome) {
        tracing::info!(
            target: "rise::audit",
            actor = %self.actor,
            subject = %self.subject(),
            operation,
            operator = self.is_operator(),
            claims = outcome.claims.len(),
            rejected = outcome.rejections.len(),
            creation_exception = outcome.creation_exception,
            "resource.grant_gate"
        );
    }
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
/// delegate and to whom, so the message names the operation, the recipient, the
/// domain, and the missing authority — and translates the algebra's synthetic
/// probe kinds, which stand for "every other kind", rather than echoing an
/// invented resource kind at someone.
fn gate_rejection_to_server_error(operation: &str, outcome: &GateOutcome) -> ServerError {
    let details: Vec<String> = outcome
        .rejections
        .iter()
        .map(render_rejection)
        .collect::<Vec<_>>();
    ServerError::forbidden(format!(
        "{operation} would grant authority you do not hold: {}",
        details.join("; ")
    ))
}

fn render_rejection(rejection: &GateRejection) -> String {
    let recipient = match &rejection.recipient {
        rise_resource_api::BindingSubject::Literal(subject) => subject.to_string(),
        template => format!("subjects matching {template}"),
    };
    match &rejection.reason {
        RejectionReason::OperatorStandingRequired => {
            format!("{recipient} would gain operator standing, which only an operator may confer")
        }
        RejectionReason::InsufficientAuthority(missing) => {
            let scope = rejection
                .domain
                .as_ref()
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
            format!(
                "{recipient} would gain {}{scope}, which you do not hold there",
                render_tuples(missing)
            )
        }
    }
}

/// The algebra enumerates equivalence classes, so a witness may be a synthetic
/// probe kind or subresource meaning "any other". Rendering those verbatim would
/// point a caller at a resource kind that does not exist.
fn render_tuples(tuples: &[PermissionTuple]) -> String {
    let mut rendered: Vec<String> = tuples
        .iter()
        .map(|tuple| {
            let kind = if is_probe_kind(&tuple.kind) {
                "any other resource kind".to_owned()
            } else {
                tuple.kind.to_string()
            };
            match &tuple.subresource {
                None => format!("{} on {kind}", verb_name(tuple.verb)),
                Some(name) if is_probe_subresource(name) => {
                    format!(
                        "{} on any other subresource of {kind}",
                        verb_name(tuple.verb)
                    )
                }
                Some(name) => format!("{} on {kind}/{name}", verb_name(tuple.verb)),
            }
        })
        .collect();
    rendered.sort();
    rendered.dedup();
    // A long witness list is a symptom, not information: the first few name the
    // shape of what is missing and the rest repeat it across kinds.
    const MAX_WITNESSES: usize = 4;
    if rendered.len() > MAX_WITNESSES {
        let remaining = rendered.len() - MAX_WITNESSES;
        rendered.truncate(MAX_WITNESSES);
        format!("{} (and {remaining} more)", rendered.join(", "))
    } else {
        rendered.join(", ")
    }
}

const PROBE_GROUP_SUFFIX: &str = ".invalid";
const PROBE_PREFIX: &str = "policy-subset-probe";

fn is_probe_kind(kind: &ResourceKind) -> bool {
    kind.group().starts_with(PROBE_PREFIX) && kind.group().ends_with(PROBE_GROUP_SUFFIX)
}

fn is_probe_subresource(name: &SubresourceName) -> bool {
    name.as_ref().starts_with(PROBE_PREFIX)
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
