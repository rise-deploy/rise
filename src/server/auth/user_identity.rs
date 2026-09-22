//! Live `User` identity for interactive logins (ADR-0001 §1, §7).
//!
//! A validated upstream login is resolved through its exact `UserIdentity
//! (issuer, subject)` mapping to the parent `User` resource, and the session
//! Rise issues names that User by canonical subject plus UID. Every later
//! request re-resolves the pair, so deactivating or deleting the User ends
//! every session already issued for it.
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
    ) -> Result<UserPrincipal, LoginError> {
        let subject = ExternalSubject::new(subject)
            .map_err(|error| LoginError::InvalidSubject(error.to_string()))?;

        // The common case — a returning user — needs no transaction.
        if let Some(principal) = IdentityLookup::new(self.pool.clone())
            .by_external_identity(&self.issuer, &subject)
            .await
            .map_err(LoginError::Store)
            .and_then(|fact| fact.map(admit).transpose())?
        {
            return Ok(principal);
        }

        let mut attempt = 1;
        loop {
            match self.provision_once(&subject, profile).await {
                Ok(principal) => return Ok(principal),
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
    ) -> Result<UserPrincipal, LoginError> {
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
        store
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
        Ok(UserPrincipal {
            name: user.name,
            uid: user.uid,
        })
    }

    /// Re-resolve a session's `(sub, rise_uid)` to the live, active User it
    /// was issued for.
    ///
    /// Both halves must name the same resource: a User deleted and recreated
    /// under the same name has a new UID, so sessions issued for the old one
    /// never revive. An inactive UserIdentity does not end the session — only
    /// the User's own `active` flag does (ADR-0001 §1).
    pub async fn resolve_session(
        &self,
        subject: &str,
        rise_uid: Uuid,
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
        let spec: UserSpec = serde_json::from_value(row.spec).map_err(|error| {
            StoreError::backend(std::io::Error::other(format!(
                "stored User '{}' is not valid: {error}",
                row.name
            )))
        })?;
        if !spec.active {
            return Err(SessionRejection::Inactive);
        }
        Ok(UserPrincipal {
            name: row.name,
            uid: row.uid,
        })
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
) -> Result<UserPrincipal, LoginError> {
    if !fact.identity.active || !fact.user.active {
        return Err(LoginError::Inactive);
    }
    Ok(UserPrincipal {
        name: fact.user_name,
        uid: fact.user_uid,
    })
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
        let error = UserLogins::new(store, pool, "https://idp.example.com/")
            .err()
            .expect("a trailing slash is not canonical");
        assert!(error.to_string().contains("auth.issuer"), "{error}");
    }

    /// ADR-0001 scenario 10
    #[sqlx::test]
    async fn a_first_login_provisions_a_user_and_its_identity(pool: PgPool) {
        let logins = logins(&pool).await;
        let user = logins
            .resolve_or_provision("subject-1", &profile("ada@example.com"))
            .await
            .expect("a first login provisions");
        assert!(user.name.starts_with("u-"), "{}", user.name);

        let row = logins.store.get(user.uid).await.unwrap().expect("the User");
        assert_eq!(row.kind, USER_KIND);
        assert_eq!(row.name, user.name);
        assert!(row.parent_uid.is_none());
        let spec: UserSpec = serde_json::from_value(row.spec).unwrap();
        assert!(spec.active);
        assert_eq!(spec.primary_email.as_deref(), Some("ada@example.com"));
        assert_eq!(spec.display_name.as_deref(), Some("Ada"));

        let rows = identities(&logins, &user).await;
        assert_eq!(rows.len(), 1);
        let identity: UserIdentitySpec = serde_json::from_value(rows[0].spec.clone()).unwrap();
        assert_eq!(identity.issuer.as_str(), ISSUER);
        assert_eq!(identity.subject.as_str(), "subject-1");
        assert!(identity.active);

        // A returning login resolves to the same User and provisions nothing.
        let again = logins
            .resolve_or_provision("subject-1", &profile("renamed@example.com"))
            .await
            .unwrap();
        assert_eq!(again, user);
        assert_eq!(identities(&logins, &user).await.len(), 1);
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
        let users: Vec<UserPrincipal> = futures::future::join_all(attempts)
            .await
            .into_iter()
            .map(|joined| joined.expect("task").expect("every attempt resolves"))
            .collect();
        assert!(
            users.iter().all(|user| user == &users[0]),
            "every concurrent login resolves to one User: {users:?}"
        );
        assert_eq!(identities(&logins, &users[0]).await.len(), 1);
        let mapped = IdentityLookup::new(pool.clone())
            .by_external_identity(
                &Issuer::new(ISSUER).unwrap(),
                &ExternalSubject::new("racing-subject").unwrap(),
            )
            .await
            .unwrap()
            .expect("the winning mapping");
        assert_eq!(mapped.user_uid, users[0].uid);
    }

    /// ADR-0001 scenario 9
    /// ADR-0001 scenario 10
    #[sqlx::test]
    async fn an_inactive_mapping_or_user_is_refused_and_never_reprovisioned(pool: PgPool) {
        let logins = logins(&pool).await;
        let user = logins
            .resolve_or_provision("subject-1", &profile("ada@example.com"))
            .await
            .unwrap();
        let identity = identities(&logins, &user).await.remove(0);

        set_active(&logins, identity.uid, false).await;
        assert!(matches!(
            logins
                .resolve_or_provision("subject-1", &profile("ada@example.com"))
                .await,
            Err(LoginError::Inactive)
        ));
        // An inactive identity does not end the User's sessions.
        assert_eq!(
            logins
                .resolve_session(user.subject().as_ref(), user.uid)
                .await
                .unwrap(),
            user
        );

        set_active(&logins, identity.uid, true).await;
        set_active(&logins, user.uid, false).await;
        assert!(matches!(
            logins
                .resolve_or_provision("subject-1", &profile("ada@example.com"))
                .await,
            Err(LoginError::Inactive)
        ));
        // An inactive User fails every session already issued for it.
        assert!(matches!(
            logins
                .resolve_session(user.subject().as_ref(), user.uid)
                .await,
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
    #[sqlx::test]
    async fn deleting_a_mapping_lets_the_next_login_provision_a_fresh_user(pool: PgPool) {
        let logins = logins(&pool).await;
        let first = logins
            .resolve_or_provision("subject-1", &profile("ada@example.com"))
            .await
            .unwrap();
        let identity = identities(&logins, &first).await.remove(0);
        logins.store.delete(identity.uid).await.unwrap();

        let second = logins
            .resolve_or_provision("subject-1", &profile("ada@example.com"))
            .await
            .unwrap();
        assert_ne!(
            second.uid, first.uid,
            "deletion is unlinking, not disablement"
        );
        assert_ne!(second.name, first.name);
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
        assert_ne!(one.uid, two.uid);
    }

    #[sqlx::test]
    async fn a_session_resolves_only_to_the_same_live_user(pool: PgPool) {
        let logins = logins(&pool).await;
        let user = logins
            .resolve_or_provision("subject-1", &profile("ada@example.com"))
            .await
            .unwrap();
        let subject = user.subject().to_string();

        assert_eq!(
            logins.resolve_session(&subject, user.uid).await.unwrap(),
            user
        );
        assert!(matches!(
            logins.resolve_session(&subject, Uuid::new_v4()).await,
            Err(SessionRejection::Unknown)
        ));
        assert!(matches!(
            logins
                .resolve_session("user:u-someone-else", user.uid)
                .await,
            Err(SessionRejection::NameMismatch)
        ));
        assert!(matches!(
            logins.resolve_session("controller:k8s", user.uid).await,
            Err(SessionRejection::NotAUser)
        ));

        // A User recreated under the same name is a new UID: the old session
        // does not revive.
        for identity in identities(&logins, &user).await {
            logins.store.delete(identity.uid).await.unwrap();
        }
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
            logins.resolve_session(&subject, user.uid).await,
            Err(SessionRejection::Unknown)
        ));
    }

    #[sqlx::test]
    async fn an_ingress_subject_follows_its_mapping(pool: PgPool) {
        let logins = logins(&pool).await;
        // No mapping: nothing to disable, so the token stands.
        assert!(logins.external_subject_active("unmapped").await.unwrap());

        let user = logins
            .resolve_or_provision("subject-1", &profile("ada@example.com"))
            .await
            .unwrap();
        assert!(logins.external_subject_active("subject-1").await.unwrap());
        set_active(&logins, user.uid, false).await;
        assert!(!logins.external_subject_active("subject-1").await.unwrap());
    }
}
