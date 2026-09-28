//! Live `User` identity for interactive logins (ADR-0001 §1, §7).
//!
//! A validated upstream login is resolved through its exact `UserIdentity
//! (issuer, subject)` mapping to the parent `User` resource, and the session
//! Rise issues names that User by canonical subject plus UID, and records the
//! identity it was minted through. Every later request re-resolves them, so
//! deactivating or deleting the User ends every session already issued for
//! it, and deactivating or deleting one identity ends the sessions minted
//! through that identity.
//!
//! **JIT provisioning is the configuration-rooted exception to grant-gated
//! identity linking.** A login whose pair has no live mapping creates a fresh
//! generated User and that first UserIdentity in one `SERIALIZABLE`
//! transaction, through the raw store rather than the authorization choke
//! point — there is no principal yet to authorize. It never attaches an
//! unknown identity to an existing User and never links by email; the new
//! User holds no grant unless restart-loaded configuration says otherwise.
//! An inactive mapping, or an active one under an inactive User, is found and
//! refused — never treated as unknown.

use std::sync::Arc;

use rise_resource_api::{
    CreateResourceParams, ExternalSubject, Issuer, ResourceApi, ResourceRow, StoreError, SubjectId,
    UserIdentitySpec, UserSpec, API_VERSION_V1ALPHA1, USER_IDENTITY_KIND, USER_KIND,
};
use rise_resource_store_postgres::{IdentityLookup, PgResourceStore, SerializableTransaction};
use sqlx::PgPool;
use uuid::Uuid;

/// Attempts at the provisioning transaction before a login gives up.
///
/// A lost race against a concurrent first login for the same pair converges on
/// the next attempt, which finds the winner's mapping; the bound only matters
/// under pathological contention.
const MAX_PROVISION_ATTEMPTS: u32 = 5;

/// The `User` resource an authenticated session belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserPrincipal {
    /// The User's canonical resource name (`u-<ulid>` when provisioned here).
    pub name: String,
    /// The User resource's UID — the session's `rise_uid`.
    pub uid: Uuid,
}

impl UserPrincipal {
    /// The canonical `user:<name>` subject.
    pub fn subject(&self) -> SubjectId {
        format!("user:{}", self.name)
            .parse()
            .expect("a stored User name is a valid subject name")
    }
}

/// What a login resolved to: the User, and the `UserIdentity` it came
/// through. A session records both, so deactivating that one identity ends
/// the sessions it minted without touching the User's other logins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedIdentity {
    pub principal: UserPrincipal,
    pub identity_uid: Uuid,
}

/// Whether a Rise-issued User token still authenticates.
#[derive(Debug)]
pub enum TokenStanding {
    /// It does. A session names its User resource; a token carrying the IdP's
    /// `sub` (legacy session, app-ingress token) names none.
    Active(Option<UserPrincipal>),
    /// It no longer does: its User or identity is inactive, gone, or replaced.
    Rejected(SessionRejection),
}

/// Non-authoritative presentation data a login supplies for a new User.
#[derive(Debug, Default, Clone)]
pub struct LoginProfile {
    pub email: Option<String>,
    pub display_name: Option<String>,
}

/// Why a login could not be resolved to an active User.
#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    /// The exact mapping, or its User, is switched off. Never JIT.
    #[error("the account is disabled")]
    Inactive,
    /// The upstream `sub` is not a subject Rise can store.
    #[error("the identity provider's subject is not acceptable: {0}")]
    InvalidSubject(String),
    #[error("identity store error: {0}")]
    Store(#[from] StoreError),
}

/// Why a session's `(sub, rise_uid)` no longer authenticates.
#[derive(Debug, thiserror::Error)]
pub enum SessionRejection {
    #[error("subject is not a User subject")]
    NotAUser,
    #[error("no live User carries this UID")]
    Unknown,
    #[error("the User carrying this UID has a different name")]
    NameMismatch,
    #[error("the User is inactive")]
    Inactive,
    #[error("no live UserIdentity of this User carries the session's identity UID")]
    IdentityUnknown,
    #[error("the UserIdentity the session was minted through is inactive")]
    IdentityInactive,
    #[error("identity store error: {0}")]
    Store(#[from] StoreError),
}

/// Resolves logins and sessions to live `User` resources.
#[derive(Clone)]
pub struct UserLogins {
    store: Arc<PgResourceStore>,
    pool: PgPool,
    issuer: Issuer,
}

impl UserLogins {
    /// `issuer` is `auth.issuer`, the one upstream IdP whose logins resolve
    /// here. It must already be in canonical form: a `UserIdentity` records
    /// the issuer exactly (ADR-0001 §1), and repairing an authentication
    /// identifier silently would make two spellings of one IdP disagree.
    pub fn new(store: Arc<PgResourceStore>, pool: PgPool, issuer: &str) -> anyhow::Result<Self> {
        let issuer = Issuer::new(issuer).map_err(|error| {
            anyhow::anyhow!("auth.issuer '{issuer}' cannot identify User logins: {error}")
        })?;
        Ok(Self {
            store,
            pool,
            issuer,
        })
    }

    /// Resolve a validated login for `subject` at the configured issuer,
    /// provisioning a new User on first sight.
    ///
    /// The caller must already have verified the upstream credential against
    /// `auth.issuer` — this trusts `subject` as that issuer's.
    pub async fn resolve_or_provision(
        &self,
        subject: &str,
        profile: &LoginProfile,
    ) -> Result<ResolvedIdentity, LoginError> {
        let subject = ExternalSubject::new(subject)
            .map_err(|error| LoginError::InvalidSubject(error.to_string()))?;

        // The common case — a returning user — needs no transaction.
        if let Some(resolved) = IdentityLookup::new(self.pool.clone())
            .by_external_identity(&self.issuer, &subject)
            .await
            .map_err(LoginError::Store)
            .and_then(|fact| fact.map(admit).transpose())?
        {
            return Ok(resolved);
        }

        let mut attempt = 1;
        loop {
            match self.provision_once(&subject, profile).await {
                Ok(resolved) => return Ok(resolved),
                // A concurrent first login for the same pair won the unique
                // mapping index (or the transaction lost the serialization
                // race over it): the next attempt reads the winner's mapping.
                Err(LoginError::Store(
                    StoreError::Conflict(_) | StoreError::NameConflict | StoreError::Serialization,
                )) if attempt < MAX_PROVISION_ATTEMPTS => attempt += 1,
                Err(error) => return Err(error),
            }
        }
    }

    async fn provision_once(
        &self,
        subject: &ExternalSubject,
        profile: &LoginProfile,
    ) -> Result<ResolvedIdentity, LoginError> {
        let tx = SerializableTransaction::begin(&self.pool).await?;
        let store = self.store.in_session(tx.session());

        // Re-read inside the transaction: the mapping may have appeared since
        // the unlocked read, and a predicate read is what serializes this
        // attempt against a concurrent one.
        if let Some(fact) = IdentityLookup::in_session(tx.session())
            .by_external_identity(&self.issuer, subject)
            .await?
        {
            return admit(fact);
        }

        let user = store
            .create(CreateResourceParams {
                api_version: API_VERSION_V1ALPHA1.to_string(),
                kind: USER_KIND.to_string(),
                name: generated_name("u"),
                spec: to_spec(&UserSpec {
                    display_name: profile.display_name.clone(),
                    primary_email: profile.email.clone(),
                    active: true,
                })?,
                ..Default::default()
            })
            .await?;
        let identity = store
            .create(CreateResourceParams {
                api_version: API_VERSION_V1ALPHA1.to_string(),
                kind: USER_IDENTITY_KIND.to_string(),
                name: generated_name("id"),
                parent_uid: Some(user.uid),
                spec: to_spec(&UserIdentitySpec {
                    issuer: self.issuer.clone(),
                    subject: subject.clone(),
                    active: true,
                })?,
                ..Default::default()
            })
            .await?;
        tx.commit().await?;

        tracing::info!(
            user = %user.name,
            user_uid = %user.uid,
            issuer = %self.issuer.as_str(),
            "Provisioned a User for a first login"
        );
        Ok(ResolvedIdentity {
            principal: UserPrincipal {
                name: user.name,
                uid: user.uid,
            },
            identity_uid: identity.uid,
        })
    }

    /// Re-resolve a session's `(sub, rise_uid, rise_identity_uid)` to the
    /// live, active User it was issued for and the live, active identity it
    /// was minted through.
    ///
    /// The subject and UID must name the same resource: a User deleted and
    /// recreated under the same name has a new UID, so sessions issued for
    /// the old one never revive. The identity must still be a live child of
    /// that User: deactivating or deleting it ends the sessions it minted, and
    /// only those — the User's other identities keep theirs (ADR-0001 §1).
    pub async fn resolve_session(
        &self,
        subject: &str,
        rise_uid: Uuid,
        identity_uid: Uuid,
    ) -> Result<UserPrincipal, SessionRejection> {
        let subject: SubjectId = subject.parse().map_err(|_| SessionRejection::NotAUser)?;
        if subject.kind() != "user" {
            return Err(SessionRejection::NotAUser);
        }
        let row = self
            .store
            .get(rise_uid)
            .await?
            .filter(is_live_user)
            .ok_or(SessionRejection::Unknown)?;
        if row.name != subject.name() {
            return Err(SessionRejection::NameMismatch);
        }
        let spec: UserSpec = parse_spec(&row)?;
        if !spec.active {
            return Err(SessionRejection::Inactive);
        }

        let identity = self
            .store
            .get(identity_uid)
            .await?
            .filter(|identity| is_live_identity_of(identity, row.uid))
            .ok_or(SessionRejection::IdentityUnknown)?;
        let identity_spec: UserIdentitySpec = parse_spec(&identity)?;
        if !identity_spec.active {
            return Err(SessionRejection::IdentityInactive);
        }
        Ok(UserPrincipal {
            name: row.name,
            uid: row.uid,
        })
    }

    /// Whether a Rise-issued User token still authenticates, for every token
    /// shape: a session names its User and identity by UID, while a legacy
    /// session or an app-ingress token carries the IdP's `sub` and is checked
    /// through that identity's mapping.
    ///
    /// `Err` is a store failure, not a verdict — callers answer it as a server
    /// error rather than logging the user out.
    pub async fn check_token(
        &self,
        subject: &str,
        uids: Option<(Uuid, Uuid)>,
    ) -> Result<TokenStanding, StoreError> {
        match uids {
            Some((rise_uid, identity_uid)) => {
                match self.resolve_session(subject, rise_uid, identity_uid).await {
                    Ok(principal) => Ok(TokenStanding::Active(Some(principal))),
                    Err(SessionRejection::Store(error)) => Err(error),
                    Err(rejection) => Ok(TokenStanding::Rejected(rejection)),
                }
            }
            None => Ok(if self.external_subject_active(subject).await? {
                TokenStanding::Active(None)
            } else {
                TokenStanding::Rejected(SessionRejection::Inactive)
            }),
        }
    }
}

impl UserLogins {
    /// Whether a token carrying the IdP's `sub` — an app-ingress token, which
    /// keeps the upstream subject for the apps that read it — may still
    /// authenticate.
    ///
    /// A mapping found for the pair must be active under an active User, the
    /// same rule a login applies. A pair with no mapping predates identity
    /// resolution (or was unlinked), and nothing ties it to a User that could
    /// have been disabled, so it stands until the token expires.
    pub async fn external_subject_active(&self, subject: &str) -> Result<bool, StoreError> {
        let Ok(subject) = ExternalSubject::new(subject) else {
            return Ok(false);
        };
        Ok(IdentityLookup::new(self.pool.clone())
            .by_external_identity(&self.issuer, &subject)
            .await?
            .is_none_or(|fact| fact.identity.active && fact.user.active))
    }
}

/// A found mapping authenticates only while both it and its User are active.
fn admit(
    fact: rise_resource_store_postgres::UserIdentityFact,
) -> Result<ResolvedIdentity, LoginError> {
    if !fact.identity.active || !fact.user.active {
        return Err(LoginError::Inactive);
    }
    Ok(ResolvedIdentity {
        principal: UserPrincipal {
            name: fact.user_name,
            uid: fact.user_uid,
        },
        identity_uid: fact.identity_uid,
    })
}

/// A stored spec that will not parse is corrupt data, not an inactive row.
fn parse_spec<T: serde::de::DeserializeOwned>(row: &ResourceRow) -> Result<T, StoreError> {
    serde_json::from_value(row.spec.clone()).map_err(|error| {
        StoreError::backend(std::io::Error::other(format!(
            "stored {} '{}' is not valid: {error}",
            row.kind, row.name
        )))
    })
}

fn is_live_identity_of(row: &ResourceRow, user_uid: Uuid) -> bool {
    row.api_version.split('/').next() == Some("rise.dev")
        && row.kind == USER_IDENTITY_KIND
        && row.parent_uid == Some(user_uid)
        && row.deletion_timestamp.is_none()
}

fn is_live_user(row: &ResourceRow) -> bool {
    row.api_version.split('/').next() == Some("rise.dev")
        && row.kind == USER_KIND
        && row.parent_uid.is_none()
        && row.deletion_timestamp.is_none()
}

fn to_spec(spec: &impl serde::Serialize) -> Result<serde_json::Value, LoginError> {
    serde_json::to_value(spec).map_err(|error| LoginError::Store(StoreError::backend(error)))
}

/// `<prefix>-<lowercase ULID>`: collision-resistant, DNS-safe, and ordered by
/// creation time, as ADR-0001 §1 names Users. The store's per-parent name
/// uniqueness remains the final authority.
fn generated_name(prefix: &str) -> String {
    format!(
        "{prefix}-{}",
        ulid(std::time::SystemTime::now(), rand_u128())
    )
}

fn rand_u128() -> u128 {
    use rand::RngExt;
    rand::rng().random()
}

/// Encode a ULID — a 48-bit millisecond timestamp followed by 80 random bits —
/// in lowercase Crockford base32 (26 characters).
fn ulid(at: std::time::SystemTime, random: u128) -> String {
    const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";
    let millis = at
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or_default()
        & ((1 << 48) - 1);
    let value = (millis << 80) | (random & ((1 << 80) - 1));
    (0..26)
        .map(|index| ALPHABET[((value >> (125 - 5 * index)) & 31) as usize] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ulid_is_26_lowercase_crockford_characters_ordered_by_time() {
        let earlier = std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_700_000_000_000);
        let later = earlier + std::time::Duration::from_millis(1);
        let first = ulid(earlier, u128::MAX);
        let second = ulid(later, 0);
        assert_eq!(first.len(), 26);
        assert!(first
            .chars()
            .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase()));
        assert!(!first.contains(['i', 'l', 'o', 'u']));
        assert!(first < second, "{first} should sort before {second}");
        // The timestamp occupies the first ten characters.
        assert_eq!(&ulid(earlier, 0)[..10], &first[..10]);
    }

    #[test]
    fn generated_names_are_valid_resource_names() {
        let name = generated_name("u");
        assert_eq!(name.len(), 28);
        rise_resource_api::validate_resource_name(&name).expect("a valid resource name");
        assert_ne!(name, generated_name("u"));
    }

    const ISSUER: &str = "https://idp.example.com";

    async fn logins(pool: &PgPool) -> UserLogins {
        rise_resource_store_postgres::run_migrations(pool)
            .await
            .expect("resource store migrations");
        UserLogins::new(
            Arc::new(PgResourceStore::new(pool.clone())),
            pool.clone(),
            ISSUER,
        )
        .expect("a canonical issuer")
    }

    fn profile(email: &str) -> LoginProfile {
        LoginProfile {
            email: Some(email.to_string()),
            display_name: Some("Ada".to_string()),
        }
    }

    async fn identities(logins: &UserLogins, user: &UserPrincipal) -> Vec<ResourceRow> {
        logins
            .store
            .list(API_VERSION_V1ALPHA1, USER_IDENTITY_KIND, Some(user.uid))
            .await
            .expect("list identities")
            .into_iter()
            .filter(|row| row.deletion_timestamp.is_none())
            .collect()
    }

    /// Replace a resource's `spec.active`.
    async fn set_active(logins: &UserLogins, uid: Uuid, active: bool) {
        let row = logins.store.get(uid).await.unwrap().expect("row");
        let mut spec = row.spec.clone();
        spec["active"] = serde_json::json!(active);
        logins
            .store
            .update(
                uid,
                rise_resource_api::UpdateResourceParams {
                    api_version: None,
                    revision: row.revision,
                    labels: row.labels.clone(),
                    annotations: Default::default(),
                    finalizers: row.finalizers.clone(),
                    owner_references: Vec::new(),
                    spec,
                    validator: None,
                },
            )
            .await
            .expect("update active");
    }

    #[tokio::test]
    async fn a_non_canonical_issuer_is_refused_at_startup() {
        // Checked before any connection is made, so a lazy pool suffices.
        let pool = PgPool::connect_lazy("postgres://localhost/unused").unwrap();
        let store = Arc::new(PgResourceStore::new(pool.clone()));
        let error = UserLogins::new(store.clone(), pool.clone(), "https://IDP.example.com")
            .err()
            .expect("an upper-case host is not canonical");
        assert!(error.to_string().contains("auth.issuer"), "{error}");
        // The IdP's own spelling is kept, trailing slash included.
        let logins = UserLogins::new(store, pool, "https://tenant.auth0.example/").unwrap();
        assert_eq!(logins.issuer.as_str(), "https://tenant.auth0.example/");
    }

    /// A second identity under `user`, as a governed linking flow would add.
    async fn link_identity(logins: &UserLogins, user: &UserPrincipal, subject: &str) -> Uuid {
        logins
            .store
            .create(CreateResourceParams {
                api_version: API_VERSION_V1ALPHA1.to_string(),
                kind: USER_IDENTITY_KIND.to_string(),
                name: generated_name("id"),
                parent_uid: Some(user.uid),
                spec: serde_json::json!({"issuer": ISSUER, "subject": subject}),
                ..Default::default()
            })
            .await
            .expect("link a second identity")
            .uid
    }

    /// Re-resolve the session a login would have minted.
    async fn session_of(
        logins: &UserLogins,
        login: &ResolvedIdentity,
    ) -> Result<UserPrincipal, SessionRejection> {
        logins
            .resolve_session(
                login.principal.subject().as_ref(),
                login.principal.uid,
                login.identity_uid,
            )
            .await
    }

    /// ADR-0001 scenario 10
    #[sqlx::test]
    async fn a_first_login_provisions_a_user_and_its_identity(pool: PgPool) {
        let logins = logins(&pool).await;
        let login = logins
            .resolve_or_provision("subject-1", &profile("ada@example.com"))
            .await
            .expect("a first login provisions");
        let user = &login.principal;
        assert!(user.name.starts_with("u-"), "{}", user.name);

        let row = logins.store.get(user.uid).await.unwrap().expect("the User");
        assert_eq!(row.kind, USER_KIND);
        assert_eq!(row.name, user.name);
        assert!(row.parent_uid.is_none());
        let spec: UserSpec = serde_json::from_value(row.spec).unwrap();
        assert!(spec.active);
        assert_eq!(spec.primary_email.as_deref(), Some("ada@example.com"));
        assert_eq!(spec.display_name.as_deref(), Some("Ada"));

        let rows = identities(&logins, user).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].uid, login.identity_uid);
        let identity: UserIdentitySpec = serde_json::from_value(rows[0].spec.clone()).unwrap();
        assert_eq!(identity.issuer.as_str(), ISSUER);
        assert_eq!(identity.subject.as_str(), "subject-1");
        assert!(identity.active);

        // A returning login resolves to the same User and identity, and
        // provisions nothing.
        let again = logins
            .resolve_or_provision("subject-1", &profile("renamed@example.com"))
            .await
            .unwrap();
        assert_eq!(again, login);
        assert_eq!(identities(&logins, user).await.len(), 1);
    }

    /// ADR-0001 scenario 10
    #[sqlx::test]
    async fn concurrent_first_logins_converge_on_one_user(pool: PgPool) {
        let logins = logins(&pool).await;
        let attempts = (0..8).map(|_| {
            let logins = logins.clone();
            tokio::spawn(async move {
                logins
                    .resolve_or_provision("racing-subject", &profile("race@example.com"))
                    .await
            })
        });
        let resolved: Vec<ResolvedIdentity> = futures::future::join_all(attempts)
            .await
            .into_iter()
            .map(|joined| joined.expect("task").expect("every attempt resolves"))
            .collect();
        assert!(
            resolved.iter().all(|login| login == &resolved[0]),
            "every concurrent login resolves to one User and identity: {resolved:?}"
        );
        assert_eq!(identities(&logins, &resolved[0].principal).await.len(), 1);
        let mapped = IdentityLookup::new(pool.clone())
            .by_external_identity(
                &Issuer::new(ISSUER).unwrap(),
                &ExternalSubject::new("racing-subject").unwrap(),
            )
            .await
            .unwrap()
            .expect("the winning mapping");
        assert_eq!(mapped.user_uid, resolved[0].principal.uid);
        assert_eq!(mapped.identity_uid, resolved[0].identity_uid);
    }

    /// ADR-0001 scenario 9
    /// ADR-0001 scenario 10
    #[sqlx::test]
    async fn an_inactive_mapping_or_user_is_refused_and_never_reprovisioned(pool: PgPool) {
        let logins = logins(&pool).await;
        let login = logins
            .resolve_or_provision("subject-1", &profile("ada@example.com"))
            .await
            .unwrap();

        set_active(&logins, login.identity_uid, false).await;
        assert!(matches!(
            logins
                .resolve_or_provision("subject-1", &profile("ada@example.com"))
                .await,
            Err(LoginError::Inactive)
        ));

        set_active(&logins, login.identity_uid, true).await;
        set_active(&logins, login.principal.uid, false).await;
        assert!(matches!(
            logins
                .resolve_or_provision("subject-1", &profile("ada@example.com"))
                .await,
            Err(LoginError::Inactive)
        ));
        // An inactive User fails every session already issued for it.
        assert!(matches!(
            session_of(&logins, &login).await,
            Err(SessionRejection::Inactive)
        ));

        // Neither refusal provisioned a replacement.
        let users = logins
            .store
            .list(API_VERSION_V1ALPHA1, USER_KIND, None)
            .await
            .unwrap();
        assert_eq!(users.len(), 1);
    }

    /// ADR-0001 scenario 10
    ///
    /// A session is bound to the identity whose login minted it: switching
    /// that identity off, or deleting it, ends those sessions, while sessions
    /// minted through the User's other identities carry on.
    #[sqlx::test]
    async fn a_session_ends_with_the_identity_that_minted_it(pool: PgPool) {
        let logins = logins(&pool).await;
        let first = logins
            .resolve_or_provision("subject-1", &profile("ada@example.com"))
            .await
            .unwrap();
        link_identity(&logins, &first.principal, "subject-2").await;
        let second = logins
            .resolve_or_provision("subject-2", &profile("ada@example.com"))
            .await
            .unwrap();
        assert_eq!(second.principal, first.principal, "one User, two logins");
        assert_ne!(second.identity_uid, first.identity_uid);

        set_active(&logins, first.identity_uid, false).await;
        assert!(matches!(
            session_of(&logins, &first).await,
            Err(SessionRejection::IdentityInactive)
        ));
        assert_eq!(
            session_of(&logins, &second).await.unwrap(),
            second.principal
        );

        logins.store.delete(second.identity_uid).await.unwrap();
        assert!(matches!(
            session_of(&logins, &second).await,
            Err(SessionRejection::IdentityUnknown)
        ));
    }

    #[sqlx::test]
    async fn a_session_cannot_borrow_another_users_identity(pool: PgPool) {
        let logins = logins(&pool).await;
        let ada = logins
            .resolve_or_provision("subject-1", &profile("ada@example.com"))
            .await
            .unwrap();
        let bob = logins
            .resolve_or_provision("subject-2", &profile("bob@example.com"))
            .await
            .unwrap();
        let forged = ResolvedIdentity {
            principal: ada.principal.clone(),
            identity_uid: bob.identity_uid,
        };
        assert!(matches!(
            session_of(&logins, &forged).await,
            Err(SessionRejection::IdentityUnknown)
        ));
    }

    /// ADR-0001 scenario 10
    #[sqlx::test]
    async fn deleting_a_mapping_lets_the_next_login_provision_a_fresh_user(pool: PgPool) {
        let logins = logins(&pool).await;
        let first = logins
            .resolve_or_provision("subject-1", &profile("ada@example.com"))
            .await
            .unwrap();
        logins.store.delete(first.identity_uid).await.unwrap();

        let second = logins
            .resolve_or_provision("subject-1", &profile("ada@example.com"))
            .await
            .unwrap();
        assert_ne!(
            second.principal.uid, first.principal.uid,
            "deletion is unlinking, not disablement"
        );
        assert_ne!(second.principal.name, first.principal.name);
    }

    /// ADR-0001 scenario 9
    /// ADR-0001 scenario 10
    #[sqlx::test]
    async fn a_shared_email_never_links_distinct_subjects(pool: PgPool) {
        let logins = logins(&pool).await;
        let one = logins
            .resolve_or_provision("subject-1", &profile("shared@example.com"))
            .await
            .unwrap();
        let two = logins
            .resolve_or_provision("subject-2", &profile("shared@example.com"))
            .await
            .unwrap();
        assert_ne!(one.principal.uid, two.principal.uid);
    }

    #[sqlx::test]
    async fn a_session_resolves_only_to_the_same_live_user(pool: PgPool) {
        let logins = logins(&pool).await;
        let login = logins
            .resolve_or_provision("subject-1", &profile("ada@example.com"))
            .await
            .unwrap();
        let user = login.principal.clone();
        let subject = user.subject().to_string();
        let identity = login.identity_uid;

        assert_eq!(session_of(&logins, &login).await.unwrap(), user);
        assert!(matches!(
            logins
                .resolve_session(&subject, Uuid::new_v4(), identity)
                .await,
            Err(SessionRejection::Unknown)
        ));
        assert!(matches!(
            logins
                .resolve_session("user:u-someone-else", user.uid, identity)
                .await,
            Err(SessionRejection::NameMismatch)
        ));
        assert!(matches!(
            logins
                .resolve_session("controller:k8s", user.uid, identity)
                .await,
            Err(SessionRejection::NotAUser)
        ));

        // A User recreated under the same name is a new UID: the old session
        // does not revive.
        logins.store.delete(identity).await.unwrap();
        logins.store.delete(user.uid).await.unwrap();
        let recreated = logins
            .store
            .create(CreateResourceParams {
                api_version: API_VERSION_V1ALPHA1.to_string(),
                kind: USER_KIND.to_string(),
                name: user.name.clone(),
                spec: serde_json::json!({}),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_ne!(recreated.uid, user.uid);
        assert!(matches!(
            session_of(&logins, &login).await,
            Err(SessionRejection::Unknown)
        ));
    }

    /// Every token shape goes through `check_token`: a session by its UIDs,
    /// and a legacy session or app-ingress token by its IdP `sub`.
    #[sqlx::test]
    async fn check_token_covers_every_token_shape(pool: PgPool) {
        let logins = logins(&pool).await;
        // An unmapped IdP subject has nothing to disable, so it stands.
        assert!(matches!(
            logins.check_token("unmapped", None).await.unwrap(),
            TokenStanding::Active(None)
        ));

        let login = logins
            .resolve_or_provision("subject-1", &profile("ada@example.com"))
            .await
            .unwrap();
        let uids = Some((login.principal.uid, login.identity_uid));
        let subject = login.principal.subject().to_string();
        assert!(matches!(
            logins.check_token(&subject, uids).await.unwrap(),
            TokenStanding::Active(Some(principal)) if principal == login.principal
        ));
        assert!(matches!(
            logins.check_token("subject-1", None).await.unwrap(),
            TokenStanding::Active(None)
        ));

        // Disabling the User ends both shapes.
        set_active(&logins, login.principal.uid, false).await;
        assert!(matches!(
            logins.check_token(&subject, uids).await.unwrap(),
            TokenStanding::Rejected(SessionRejection::Inactive)
        ));
        assert!(matches!(
            logins.check_token("subject-1", None).await.unwrap(),
            TokenStanding::Rejected(_)
        ));
    }

    /// A store that cannot answer is an error for the caller to report, never
    /// a verdict that logs the user out.
    #[sqlx::test]
    async fn check_token_reports_store_failures_as_errors(pool: PgPool) {
        let logins = logins(&pool).await;
        let login = logins
            .resolve_or_provision("subject-1", &profile("ada@example.com"))
            .await
            .unwrap();
        pool.close().await;

        let subject = login.principal.subject().to_string();
        let uids = Some((login.principal.uid, login.identity_uid));
        assert!(logins.check_token(&subject, uids).await.is_err());
        assert!(logins.check_token("subject-1", None).await.is_err());
    }
}
