//! Microsoft Entra ID active sync
//!
//! Periodically pulls users and groups assigned to the configured Entra app registration
//! via the Microsoft Graph API and syncs them as Rise teams. This provides server-side
//! synchronization without requiring user logins or SCIM ingress.
//!
//! ## Required Entra permissions
//!
//! The app registration must have the following Microsoft Graph **Application** permissions:
//! - `Application.Read.All` — to look up the service principal
//! - `GroupMember.Read.All` — to read group members
//! - `User.Read.All` — to read user details
//!
//! ## How it works
//!
//! 1. Authenticates to Microsoft Graph using client credentials
//! 2. Looks up the service principal for the configured `client_id`
//! 3. Lists all app role assignments (users and groups assigned to the enterprise app)
//! 4. For each assigned group, fetches members and syncs as an IdP-managed Rise team
//! 5. Removes members from Rise teams that are no longer assigned in Entra

use anyhow::{Context, Result};
use serde::Deserialize;
use sqlx::PgPool;
use std::collections::HashSet;
use std::time::Duration;
use uuid::Uuid;

use crate::db::{models::TeamRole, teams, users};
use rise_runtime_sync::{GlobalSchedule, LeaderElection};

// ============================================================================
// Microsoft Graph API types
// ============================================================================

/// OAuth2 token response from Azure AD
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[allow(dead_code)]
    expires_in: u64,
}

/// Microsoft Graph paginated response wrapper
#[derive(Debug, Deserialize)]
struct GraphListResponse<T> {
    value: Vec<T>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
}

/// Service principal object from Microsoft Graph
#[derive(Debug, Deserialize)]
struct ServicePrincipal {
    id: String,
}

/// App role assignment from Microsoft Graph
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppRoleAssignment {
    principal_id: String,
    principal_type: String,
    principal_display_name: Option<String>,
}

/// User object from Microsoft Graph (minimal fields)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphUser {
    #[allow(dead_code)]
    id: String,
    mail: Option<String>,
    user_principal_name: Option<String>,
}

// ============================================================================
// Graph API client
// ============================================================================

/// Client for Microsoft Graph API operations
struct GraphClient {
    http: reqwest::Client,
    tenant_id: String,
    client_id: String,
    client_secret: String,
    access_token: Option<String>,
}

impl GraphClient {
    fn new(tenant_id: &str, client_id: &str, client_secret: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            tenant_id: tenant_id.to_string(),
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
            access_token: None,
        }
    }

    /// Acquire an access token using the client credentials flow
    async fn ensure_token(&mut self) -> Result<()> {
        let token_url = format!(
            "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
            self.tenant_id
        );

        let resp = self
            .http
            .post(&token_url)
            .form(&[
                ("grant_type", "client_credentials"),
                ("client_id", &self.client_id),
                ("client_secret", &self.client_secret),
                ("scope", "https://graph.microsoft.com/.default"),
            ])
            .send()
            .await
            .context("Failed to request Graph API token")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Token request failed (HTTP {}): {}",
                status,
                truncate_string(&body, 500)
            );
        }

        let token: TokenResponse = resp
            .json()
            .await
            .context("Failed to parse token response")?;

        self.access_token = Some(token.access_token);
        Ok(())
    }

    fn token(&self) -> Result<&str> {
        self.access_token
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("No access token available"))
    }

    /// Fetch a paginated list from a Graph API endpoint, following `@odata.nextLink`
    async fn get_paginated<T: serde::de::DeserializeOwned>(
        &self,
        initial_url: &str,
    ) -> Result<Vec<T>> {
        let mut results = Vec::new();
        let mut url = initial_url.to_string();

        loop {
            let resp = self
                .http
                .get(&url)
                .bearer_auth(self.token()?)
                .header("ConsistencyLevel", "eventual")
                .send()
                .await
                .with_context(|| format!("Graph API request failed for {}", url))?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!(
                    "Graph API error (HTTP {}) for {}: {}",
                    status,
                    url,
                    truncate_string(&body, 500)
                );
            }

            let page: GraphListResponse<T> = resp
                .json()
                .await
                .with_context(|| format!("Failed to parse Graph API response from {}", url))?;

            results.extend(page.value);

            match page.next_link {
                Some(next) => url = next,
                None => break,
            }
        }

        Ok(results)
    }

    /// Look up the service principal by app ID (client_id)
    async fn get_service_principal_id(&self) -> Result<String> {
        let encoded_client_id = urlencoding::encode(&self.client_id);
        let url = format!(
            "https://graph.microsoft.com/v1.0/servicePrincipals?$filter=appId eq '{}'&$select=id",
            encoded_client_id
        );

        let sps: Vec<ServicePrincipal> = self.get_paginated(&url).await?;

        sps.into_iter()
            .next()
            .map(|sp| sp.id)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "No service principal found for appId '{}'. Ensure the app is registered as an Enterprise Application in Entra.",
                    self.client_id
                )
            })
    }

    /// List all app role assignments (users and groups assigned to the enterprise app)
    async fn get_app_role_assignments(&self, sp_id: &str) -> Result<Vec<AppRoleAssignment>> {
        let encoded_sp_id = urlencoding::encode(sp_id);
        let url = format!(
            "https://graph.microsoft.com/v1.0/servicePrincipals/{}/appRoleAssignedTo?$select=principalId,principalType,principalDisplayName",
            encoded_sp_id
        );
        self.get_paginated(&url).await
    }

    /// Get transitive user members of a group (resolves nested groups)
    async fn get_group_user_members(&self, group_id: &str) -> Result<Vec<GraphUser>> {
        let encoded_group_id = urlencoding::encode(group_id);
        let url = format!(
            "https://graph.microsoft.com/v1.0/groups/{}/transitiveMembers/microsoft.graph.user?$select=id,mail,userPrincipalName",
            encoded_group_id
        );
        self.get_paginated(&url).await
    }
}

// ============================================================================
// Sync logic
// ============================================================================

/// Information about a group to sync
struct GroupToSync {
    display_name: String,
    team_name: String,
    member_emails: Vec<String>,
}

/// Extract tenant ID from an Entra issuer URL.
///
/// Supported formats:
/// - `https://login.microsoftonline.com/{tenant}/v2.0`
/// - `https://login.microsoftonline.com/{tenant}`
/// - `https://sts.windows.net/{tenant}/`
pub fn extract_tenant_id(issuer: &str) -> Result<String> {
    // Remove trailing slashes and /v2.0 suffix
    let normalized = issuer
        .trim_end_matches('/')
        .trim_end_matches("/v2.0")
        .trim_end_matches('/');

    // Extract the last path segment
    let tenant = normalized
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Cannot extract tenant ID from issuer URL '{}'. \
                 Expected format: https://login.microsoftonline.com/{{tenant}}/v2.0",
                issuer
            )
        })?;

    Ok(tenant.to_string())
}

/// Sanitize an Entra group display name into a valid Rise team name.
///
/// Rise team names must match `^[a-z0-9-]+$`. This function:
/// - Converts to lowercase
/// - Replaces spaces, underscores, and dots with hyphens
/// - Removes characters that are not `[a-z0-9-]`
/// - Collapses multiple consecutive hyphens
/// - Trims leading/trailing hyphens
pub fn sanitize_team_name(display_name: &str) -> String {
    let mut result = String::with_capacity(display_name.len());

    for ch in display_name.chars() {
        match ch {
            'A'..='Z' => result.push(ch.to_ascii_lowercase()),
            'a'..='z' | '0'..='9' => result.push(ch),
            ' ' | '_' | '.' => result.push('-'),
            '-' => result.push('-'),
            _ => {} // Drop other characters
        }
    }

    // Collapse multiple consecutive hyphens
    let mut collapsed = String::with_capacity(result.len());
    let mut prev_hyphen = false;
    for ch in result.chars() {
        if ch == '-' {
            if !prev_hyphen {
                collapsed.push('-');
            }
            prev_hyphen = true;
        } else {
            collapsed.push(ch);
            prev_hyphen = false;
        }
    }

    // Trim leading/trailing hyphens
    collapsed.trim_matches('-').to_string()
}

/// Run a single sync iteration: fetch from Entra and update Rise DB
async fn sync_once(
    pool: &PgPool,
    client: &mut GraphClient,
    election: &LeaderElection,
    default_organization_uid: Uuid,
) -> Result<()> {
    // Step 1: Get a fresh token
    client.ensure_token().await?;

    // Step 2: Find the service principal
    let sp_id = client.get_service_principal_id().await?;
    tracing::debug!("Found service principal: {}", sp_id);

    // Step 3: Get all app role assignments
    let assignments = client.get_app_role_assignments(&sp_id).await?;
    tracing::info!(
        "Fetched {} app role assignments from Entra",
        assignments.len()
    );

    // Step 4: Separate group and user assignments
    let mut groups_to_sync: Vec<GroupToSync> = Vec::new();
    let mut seen_team_names: HashSet<String> = HashSet::new();

    for assignment in &assignments {
        if assignment.principal_type != "Group" {
            continue;
        }

        let display_name = assignment
            .principal_display_name
            .clone()
            .unwrap_or_else(|| assignment.principal_id.clone());

        let team_name = sanitize_team_name(&display_name);
        if team_name.is_empty() {
            tracing::warn!(
                "Skipping group '{}' — sanitized name is empty",
                display_name
            );
            continue;
        }

        if !seen_team_names.insert(team_name.clone()) {
            tracing::warn!(
                "Skipping duplicate group '{}' (sanitized to '{}' which was already seen)",
                display_name,
                team_name
            );
            continue;
        }

        // Fetch group members
        let members = match client
            .get_group_user_members(&assignment.principal_id)
            .await
        {
            Ok(m) => m,
            Err(e) => {
                tracing::error!(
                    "Failed to fetch members for group '{}' ({}): {:?}",
                    display_name,
                    assignment.principal_id,
                    e
                );
                continue;
            }
        };

        let member_emails: Vec<String> = members
            .into_iter()
            .filter_map(|u| u.mail.or(u.user_principal_name).filter(|e| !e.is_empty()))
            .collect();

        tracing::debug!(
            "Group '{}' → team '{}' with {} members",
            display_name,
            team_name,
            member_emails.len()
        );

        groups_to_sync.push(GroupToSync {
            display_name,
            team_name,
            member_emails,
        });
    }

    // Step 5: Sync groups to Rise within a transaction
    let synced_team_names: HashSet<String> =
        groups_to_sync.iter().map(|g| g.team_name.clone()).collect();

    let mut tx = pool
        .begin()
        .await
        .context("Failed to begin transaction for Entra sync")?;

    election.assert_leader().await?;

    for group in &groups_to_sync {
        if let Err(e) = sync_group(&mut tx, group, default_organization_uid).await {
            tracing::error!(
                "Failed to sync group '{}' (team '{}'): {:?}",
                group.display_name,
                group.team_name,
                e
            );
            // Transaction is rolled back implicitly when `tx` is dropped
            return Err(e);
        }
    }

    // Step 6: Clean up IdP-managed teams that are no longer assigned in Entra
    let all_idp_teams = teams::list_idp_managed(&mut *tx)
        .await
        .context("Failed to list IdP-managed teams")?;

    for team in all_idp_teams {
        if !synced_team_names.contains(&team.name) {
            // This IdP-managed team is no longer assigned in Entra — remove all members
            let removed = teams::remove_all_team_members(&mut *tx, team.id)
                .await
                .context("Failed to remove members during cleanup")?;

            if removed > 0 {
                tracing::info!(
                    "Removed {} members from IdP-managed team '{}' (no longer assigned in Entra)",
                    removed,
                    team.name
                );
            }
        }
    }

    tx.commit()
        .await
        .context("Failed to commit Entra sync transaction")?;

    tracing::info!(
        "Entra active sync completed: {} groups synced",
        groups_to_sync.len()
    );

    Ok(())
}

/// Sync a single group as a Rise team within an ongoing transaction
async fn sync_group(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    group: &GroupToSync,
    default_organization_uid: Uuid,
) -> Result<()> {
    // Find or create the team
    let existing = teams::find_by_name(&mut **tx, &group.team_name)
        .await
        .context("Failed to look up team")?;

    let team_id = if let Some(team) = existing {
        // Update name if case differs
        if team.name != group.team_name {
            teams::update_name(&mut **tx, team.id, &group.team_name)
                .await
                .context("Failed to update team name")?;
        }

        // Convert to IdP-managed if needed
        if !team.idp_managed {
            tracing::info!(
                "Converting team '{}' to IdP-managed (Entra sync)",
                group.team_name
            );
            teams::set_idp_managed(&mut **tx, team.id, true)
                .await
                .context("Failed to set IdP-managed flag")?;
            teams::remove_all_owners(&mut **tx, team.id)
                .await
                .context("Failed to remove owners on IdP takeover")?;
        }

        team.id
    } else {
        tracing::info!(
            "Creating new IdP-managed team '{}' (from Entra group '{}')",
            group.team_name,
            group.display_name
        );
        let new_team = teams::create(&mut **tx, &group.team_name)
            .await
            .context("Failed to create team")?;
        teams::set_idp_managed(&mut **tx, new_team.id, true)
            .await
            .context("Failed to set IdP-managed flag on new team")?;
        // Stamp default-Organization linkage on the freshly created team so
        // bootstrap validation never observes a team without one.
        crate::db::organization_links::set_team_organization(
            tx,
            new_team.id,
            default_organization_uid,
        )
        .await
        .context("Failed to stamp default Organization on new team")?;
        new_team.id
    };

    // Resolve member emails to user IDs (create users if needed). Each new
    // user also gets a default-Organization membership in the same
    // transaction so bootstrap validation never observes a user without one.
    let mut expected_user_ids: HashSet<Uuid> = HashSet::new();
    for email in &group.member_emails {
        let user = users::find_or_create_with_executor(tx, email)
            .await
            .with_context(|| format!("Failed to find/create user '{}'", email))?;
        crate::db::organization_links::ensure_user_membership(
            tx,
            user.id,
            default_organization_uid,
        )
        .await
        .with_context(|| format!("Failed to ensure default-Org membership for '{}'", email))?;
        expected_user_ids.insert(user.id);
    }

    // Get current team members
    let current_member_ids: HashSet<Uuid> = teams::get_all_member_user_ids(&mut **tx, team_id)
        .await
        .context("Failed to get current team members")?
        .into_iter()
        .collect();

    // Add missing members
    for uid in &expected_user_ids {
        if !current_member_ids.contains(uid) {
            teams::add_member(&mut **tx, team_id, *uid, TeamRole::Member)
                .await
                .with_context(|| format!("Failed to add member {} to team", uid))?;
        }
    }

    // Remove members who should no longer be in the team
    for uid in &current_member_ids {
        if !expected_user_ids.contains(uid) {
            teams::remove_all_user_roles(&mut **tx, team_id, *uid)
                .await
                .with_context(|| format!("Failed to remove member {} from team", uid))?;
        }
    }

    Ok(())
}

/// Run the Entra active sync background loop
pub async fn run_entra_sync_loop(
    pool: PgPool,
    auth_settings: crate::server::settings::AuthSettings,
    default_organization_uid: Uuid,
) {
    let tenant_id = match extract_tenant_id(&auth_settings.issuer) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("Entra active sync cannot start: {:?}", e);
            return;
        }
    };

    let interval_secs = auth_settings.active_sync_interval_secs;
    let mut client = GraphClient::new(
        &tenant_id,
        &auth_settings.client_id,
        &auth_settings.client_secret,
    );
    let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));

    tracing::info!(
        "Entra active sync started (tenant={}, interval={}s)",
        tenant_id,
        interval_secs
    );

    let mut shutdown = std::pin::pin!(async {
        let ctrl_c = tokio::signal::ctrl_c();

        #[cfg(unix)]
        let terminate = async {
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("failed to install SIGTERM handler")
                .recv()
                .await;
        };

        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();

        tokio::select! {
            _ = ctrl_c => {}
            _ = terminate => {}
        }
    });

    let election = LeaderElection::spawn(
        pool.clone(),
        "rise-entra-sync",
        Uuid::new_v4(),
        Duration::from_secs(interval_secs + 30),
    );
    let schedule = GlobalSchedule::new(
        pool.clone(),
        "rise-entra-sync-cycle",
        Duration::from_secs(interval_secs),
    );

    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = &mut shutdown => {
                tracing::info!("Entra active sync shutting down");
                break;
            }
        }

        if !election.is_leader() {
            tracing::debug!("Skipping Entra sync cycle — another replica is the leader");
            continue;
        }

        // Global cadence gate: see `GlobalSchedule` docs. Long sync intervals
        // (often 1h+) make leader-transition bursts much more visible than
        // for short-cadence workers, so this matters especially here.
        // `_as_leader` re-verifies leadership against the DB before the
        // schedule UPSERT, so a stale-cached non-leader cannot poison
        // `last_run_at` and delay the true leader by the full interval.
        if !schedule
            .try_claim_or_skip_as_leader("Entra sync", &election)
            .await
        {
            continue;
        }

        tracing::debug!("Running Entra active sync cycle");
        if let Err(e) = sync_once(&pool, &mut client, &election, default_organization_uid).await {
            tracing::error!("Entra active sync failed: {:?}", e);
        }
        tracing::info!("Next Entra active sync in {}s", interval_secs);
    }
}

/// Truncate a string for logging purposes (UTF-8 safe)
fn truncate_string(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        return s;
    }
    // Find the last valid char boundary at or before max_len
    let mut end = max_len;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_tenant_id_v2() {
        let result =
            extract_tenant_id("https://login.microsoftonline.com/my-tenant-id/v2.0").unwrap();
        assert_eq!(result, "my-tenant-id");
    }

    #[test]
    fn test_extract_tenant_id_no_v2() {
        let result = extract_tenant_id("https://login.microsoftonline.com/my-tenant-id").unwrap();
        assert_eq!(result, "my-tenant-id");
    }

    #[test]
    fn test_extract_tenant_id_sts() {
        let result = extract_tenant_id("https://sts.windows.net/my-tenant-id/").unwrap();
        assert_eq!(result, "my-tenant-id");
    }

    #[test]
    fn test_extract_tenant_id_with_trailing_slashes() {
        let result = extract_tenant_id("https://login.microsoftonline.com/abc-123/v2.0/").unwrap();
        assert_eq!(result, "abc-123");
    }

    #[test]
    fn test_extract_tenant_id_guid() {
        let result = extract_tenant_id(
            "https://login.microsoftonline.com/550e8400-e29b-41d4-a716-446655440000/v2.0",
        )
        .unwrap();
        assert_eq!(result, "550e8400-e29b-41d4-a716-446655440000");
    }

    #[test]
    fn test_extract_tenant_id_custom_domain() {
        let result =
            extract_tenant_id("https://login.microsoftonline.com/contoso.onmicrosoft.com/v2.0")
                .unwrap();
        assert_eq!(result, "contoso.onmicrosoft.com");
    }

    #[test]
    fn test_sanitize_simple() {
        assert_eq!(sanitize_team_name("engineering"), "engineering");
    }

    #[test]
    fn test_sanitize_uppercase() {
        assert_eq!(sanitize_team_name("Engineering"), "engineering");
    }

    #[test]
    fn test_sanitize_spaces() {
        assert_eq!(sanitize_team_name("Engineering Team"), "engineering-team");
    }

    #[test]
    fn test_sanitize_underscores() {
        assert_eq!(sanitize_team_name("dev_ops"), "dev-ops");
    }

    #[test]
    fn test_sanitize_dots() {
        assert_eq!(sanitize_team_name("team.name"), "team-name");
    }

    #[test]
    fn test_sanitize_special_chars() {
        assert_eq!(sanitize_team_name("Team (Special) #1!"), "team-special-1");
    }

    #[test]
    fn test_sanitize_multiple_hyphens() {
        assert_eq!(sanitize_team_name("a---b"), "a-b");
    }

    #[test]
    fn test_sanitize_leading_trailing_hyphens() {
        assert_eq!(sanitize_team_name("-team-"), "team");
    }

    #[test]
    fn test_sanitize_mixed() {
        assert_eq!(
            sanitize_team_name("Rise DevOps - Production"),
            "rise-devops-production"
        );
    }

    #[test]
    fn test_sanitize_empty_result() {
        assert_eq!(sanitize_team_name("!!!"), "");
    }

    #[test]
    fn test_sanitize_numbers() {
        assert_eq!(sanitize_team_name("Team 42"), "team-42");
    }
}
