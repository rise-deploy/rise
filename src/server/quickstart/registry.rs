use super::models::QuickstartTemplate;
use crate::server::settings::{QuickstartSettings, QuickstartTemplateConfig};

/// Convert the operator-configured catalog (`settings.quickstart`) into the
/// API model returned by `GET /quickstart-templates`. The wire shape differs
/// from the config shape only in one field: `icon` (config) is resolved into
/// `icon_url` (API), letting operators reference built-in icons by short name
/// (`welcome`) and external icons by URL or static path.
pub fn from_settings(settings: Option<&QuickstartSettings>) -> Vec<QuickstartTemplate> {
    let Some(qs) = settings else {
        return Vec::new();
    };
    qs.templates.iter().map(to_api_template).collect()
}

fn to_api_template(cfg: &QuickstartTemplateConfig) -> QuickstartTemplate {
    QuickstartTemplate {
        id: cfg.id.clone(),
        display_name: cfg.display_name.clone(),
        tagline: cfg.tagline.clone(),
        description: cfg.description.clone(),
        icon_url: resolve_icon_url(&cfg.icon),
        image: cfg.image.clone(),
        http_port: cfg.http_port,
        learn_more_url: cfg.learn_more_url.clone(),
        tags: cfg.tags.clone(),
        warning: cfg.warning.clone(),
    }
}

/// `http://...`, `https://...`, or `/some/path` are passed through verbatim.
/// Anything else is treated as a built-in icon name and resolved against
/// `/assets/quickstart/<name>.svg`, where the SVGs are shipped under the
/// backend's static dir.
fn resolve_icon_url(icon: &str) -> String {
    if icon.starts_with("http://") || icon.starts_with("https://") || icon.starts_with('/') {
        icon.to_string()
    } else {
        format!("/assets/quickstart/{}.svg", icon)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_built_in_icon_name() {
        assert_eq!(
            resolve_icon_url("welcome"),
            "/assets/quickstart/welcome.svg"
        );
    }

    #[test]
    fn passes_through_absolute_url() {
        assert_eq!(
            resolve_icon_url("https://example.com/i.svg"),
            "https://example.com/i.svg"
        );
        assert_eq!(
            resolve_icon_url("http://example.com/i.svg"),
            "http://example.com/i.svg"
        );
    }

    #[test]
    fn passes_through_static_path() {
        assert_eq!(resolve_icon_url("/custom/i.svg"), "/custom/i.svg");
    }

    #[test]
    fn empty_settings_yields_empty_catalog() {
        assert!(from_settings(None).is_empty());
    }
}
