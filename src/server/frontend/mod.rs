pub mod routes;

use std::path::{Component, Path, PathBuf};

use axum::http::StatusCode;
use tera::Tera;

/// Names of shared Tera partials that should be registered alongside any
/// auth-page template so `{% include %}` statements resolve.
const AUTH_TEMPLATE_PARTIALS: &[&str] = &["_auth-tokens.css.tera", "_auth-theme.js.tera"];

/// Load a static file from the configured static_dir, with path traversal protection.
pub async fn load_static_file(static_dir: &str, rel_path: &str) -> Option<Vec<u8>> {
    let mut safe_path = PathBuf::new();
    for part in Path::new(rel_path).components() {
        match part {
            Component::Normal(seg) => safe_path.push(seg),
            _ => return None,
        }
    }
    let full_path = PathBuf::from(static_dir).join(safe_path);
    tokio::fs::read(&full_path).await.ok()
}

/// Load a Tera template from `static_dir` and register it — along with the
/// shared partials in `AUTH_TEMPLATE_PARTIALS` — into a fresh `Tera` instance.
///
/// This is the canonical way to render the auth-page templates: each template
/// uses shared `{% include %}` partials for auth-page styling and scripts, so
/// those partials must be registered alongside.
pub async fn load_auth_template(
    static_dir: &str,
    template_name: &str,
) -> Result<Tera, (StatusCode, String)> {
    async fn load_text(static_dir: &str, name: &str) -> Result<String, (StatusCode, String)> {
        let bytes = load_static_file(static_dir, name).await.ok_or_else(|| {
            tracing::error!("{name} template not found");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Template not found".to_string(),
            )
        })?;
        std::str::from_utf8(&bytes)
            .map(|s| s.to_string())
            .map_err(|e| {
                tracing::error!("Failed to parse {name} as UTF-8: {:#}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Template encoding error".to_string(),
                )
            })
    }

    let mut tera = Tera::default();
    // Tera only escapes names ending in `.html`; these templates render
    // request-supplied values (project names, error messages), so escape them.
    tera.autoescape_on(vec![".html.tera"]);
    for partial in AUTH_TEMPLATE_PARTIALS {
        let body = load_text(static_dir, partial).await?;
        tera.add_raw_template(partial, &body).map_err(|e| {
            tracing::error!("Failed to parse partial {partial}: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Template error".to_string(),
            )
        })?;
    }
    let body = load_text(static_dir, template_name).await?;
    tera.add_raw_template(template_name, &body).map_err(|e| {
        tracing::error!("Failed to parse template {template_name}: {:#}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Template error".to_string(),
        )
    })?;
    Ok(tera)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATIC_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/static");
    const HOSTILE: &str = "<script>alert(1)</script>";

    async fn render(template: &str, values: &[(&str, &str)]) -> String {
        let tera = load_auth_template(STATIC_DIR, template).await.unwrap();
        let mut context = tera::Context::new();
        context.insert("success", &false);
        for (key, value) in values {
            context.insert(*key, value);
        }
        tera.render(template, &context).unwrap()
    }

    #[tokio::test]
    async fn auth_templates_escape_request_values() {
        for template in [
            "cli-auth-success.html.tera",
            "auth-success.html.tera",
            "auth-signin.html.tera",
        ] {
            let html = render(
                template,
                &[
                    ("error_message", HOSTILE),
                    ("project_name", HOSTILE),
                    ("redirect_url", HOSTILE),
                    ("continue_url", HOSTILE),
                ],
            )
            .await;
            assert!(!html.contains(HOSTILE), "{template} renders raw HTML");
            assert!(
                html.contains("&lt;script&gt;"),
                "{template} renders no value"
            );
        }
    }

    #[tokio::test]
    async fn redirect_script_keeps_a_json_string() {
        let tera = load_auth_template(STATIC_DIR, "auth-ui-success.html.tera")
            .await
            .unwrap();
        let mut context = tera::Context::new();
        context.insert("redirect_url", "https://app.example.com/a?b=1&c=2");
        let html = tera.render("auth-ui-success.html.tera", &context).unwrap();
        assert!(html.contains(r#"const redirectUrl = "https://app.example.com/a?b=1&c=2";"#));
    }
}
