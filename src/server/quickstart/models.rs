use serde::{Deserialize, Serialize};

/// Metadata for a quickstart template: a curated, stateless public container image
/// users can deploy in one click from the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuickstartTemplate {
    /// Stable identifier used in URLs and frontend lookups (e.g. `welcome`, `whoami`).
    pub id: String,
    /// Short, human-friendly name shown in cards and dialogs.
    pub display_name: String,
    /// One-line description for the card.
    pub tagline: String,
    /// Longer description shown in the deploy dialog.
    pub description: String,
    /// Path to the icon under `static/assets/` (e.g. `/assets/quickstart/welcome.svg`).
    pub icon_url: String,
    /// Fully-qualified, tag-pinned container image to deploy.
    pub image: String,
    /// Port the container listens on. Must be `>= 1024` so the deployment runs
    /// on runtimes that disallow binding to privileged ports without root.
    pub http_port: u16,
    /// Upstream link for users who want to learn more about what they're deploying.
    pub learn_more_url: String,
    /// Free-form tags for categorisation / filtering in future iterations.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Optional caveat shown to the user in the deploy modal — used when an
    /// image violates the non-root / unprivileged-port invariant and may fail
    /// on hardened runtimes, but is still kept in the catalog for clusters
    /// that allow it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ListQuickstartTemplatesResponse {
    pub templates: Vec<QuickstartTemplate>,
}
