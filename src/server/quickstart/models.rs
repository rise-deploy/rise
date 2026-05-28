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
    /// Port the container listens on.
    pub http_port: u16,
    /// Upstream link for users who want to learn more about what they're deploying.
    pub learn_more_url: String,
    /// Free-form tags for categorisation / filtering in future iterations.
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ListQuickstartTemplatesResponse {
    pub templates: Vec<QuickstartTemplate>,
}
