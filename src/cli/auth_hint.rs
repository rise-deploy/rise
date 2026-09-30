//! Service-account setup guidance for rejected external OIDC credentials.

use reqwest::{header::AUTHORIZATION, RequestBuilder, Response, StatusCode};
use serde_json::Value;

/// Send a backend request and report setup guidance on authentication or
/// permission failures, preserving the response for the caller's error handling.
pub trait AuthHintRequest {
    fn send_with_auth_hint(
        self,
        backend_url: &str,
    ) -> impl std::future::Future<Output = reqwest::Result<Response>> + Send;
}

impl AuthHintRequest for RequestBuilder {
    async fn send_with_auth_hint(self, backend_url: &str) -> reqwest::Result<Response> {
        let (client, request) = self.build_split();
        let request = request?;
        let token = request
            .headers()
            .get(AUTHORIZATION)
            .and_then(|header| header.to_str().ok())
            .and_then(|header| header.strip_prefix("Bearer "))
            .map(str::to_owned);
        let project = request
            .url()
            .path()
            .split_once("/api/v1/projects/")
            .and_then(|(_, path)| path.split('/').next())
            .filter(|project| !project.is_empty())
            .and_then(|project| urlencoding::decode(project).ok())
            .map(|project| project.into_owned());
        let response = client.execute(request).await?;
        if matches!(
            response.status(),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ) {
            if let Some(token) = token {
                log_service_account_hint(&token, backend_url, project.as_deref());
            }
        }
        Ok(response)
    }
}

pub fn log_service_account_hint(token: &str, backend_url: &str, project: Option<&str>) {
    if let Some(hint) = service_account_hint(token, backend_url, project) {
        tracing::warn!("{hint}");
    }
}

fn service_account_hint(token: &str, backend_url: &str, project: Option<&str>) -> Option<String> {
    // Unverified claims are diagnostic input only; the backend authenticates tokens.
    let data = jsonwebtoken::dangerous::insecure_decode::<Value>(token).ok()?;
    let issuer = data.claims.get("iss")?.as_str()?;
    if issuer.trim_end_matches('/') == backend_url.trim_end_matches('/')
        || matches!(
            data.header.typ.as_deref(),
            Some("rise-access+jwt" | "rise-identity+jwt" | "rise-session+jwt")
        )
    {
        return None;
    }
    let audience = match data.claims.get("aud")? {
        Value::String(aud) => aud.as_str(),
        Value::Array(audiences) => audiences
            .iter()
            .filter_map(Value::as_str)
            .find(|aud| *aud == backend_url)
            .or_else(|| {
                audiences
                    .iter()
                    .filter_map(Value::as_str)
                    .find(|aud| !aud.is_empty())
            })?,
        _ => return None,
    };
    let subject = data
        .claims
        .get("sub")
        .and_then(Value::as_str)
        .filter(|sub| !sub.is_empty())
        .unwrap_or("<subject>");
    let project = project.unwrap_or("<project>");
    if [issuer, audience, subject, project, backend_url]
        .iter()
        .any(|value| value.is_empty() || value.chars().any(char::is_control))
    {
        return None;
    }
    let command =
        format!(
        "RISE_URL={} rise service-account create --project {} --issuer {} --claim {} --claim {}",
        shell_quote(backend_url), shell_quote(project), shell_quote(issuer),
        shell_quote(&format!("aud={audience}")), shell_quote(&format!("sub={subject}")),
    );
    Some(format!(
        "The external OIDC token was denied access. If it needs a project service account, \
         ask a project owner to review these token claims and run this example using their own login \
         (with RISE_TOKEN, RISE_TOKEN_COMMAND, RISE_IDENTITY and GitHub Actions token variables unset):\n  \
         {command}\n\
         The aud claim is required; use the token's sub claim to restrict which identity can access the project. \
         Replace any placeholders before running."
    ))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use tracing::instrument::WithSubscriber;

    const BACKEND: &str = "https://rise.example.com";

    fn jwt(claims: Value, typ: &str) -> String {
        format!(
            "{}.{}.signature",
            URL_SAFE_NO_PAD.encode(json!({"alg": "RS256", "typ": typ}).to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        )
    }

    fn claims() -> Value {
        json!({"iss": "https://token.actions.githubusercontent.com", "aud": BACKEND, "sub": "repo:org/repo:ref:refs/heads/main", "private": "do-not-log"})
    }

    #[test]
    fn example_uses_issuer_audience_and_subject_without_leaking_token() {
        let token = jwt(claims(), "JWT");
        let hint = service_account_hint(&token, BACKEND, Some("demo")).unwrap();
        assert!(hint.contains("RISE_URL='https://rise.example.com' rise service-account create --project 'demo' --issuer 'https://token.actions.githubusercontent.com' --claim 'aud=https://rise.example.com' --claim 'sub=repo:org/repo:ref:refs/heads/main'"));
        assert!(hint.contains("project owner"));
        assert!(!hint.contains(&token));
        assert!(!hint.contains("signature"));
        assert!(!hint.contains("do-not-log"));
    }

    #[test]
    fn audience_arrays_choose_backend_or_first_nonempty_audience() {
        for (aud, expected) in [
            (json!(["other", BACKEND]), BACKEND),
            (json!(["", "ci-audience", "other"]), "ci-audience"),
        ] {
            let mut c = claims();
            c["aud"] = aud;
            let hint = service_account_hint(&jwt(c, "JWT"), BACKEND, None).unwrap();
            assert!(hint.contains(&format!("--claim 'aud={expected}'")));
            assert!(hint.contains("--project '<project>'"));
        }
    }

    #[test]
    fn rise_tokens_have_no_setup_hint() {
        for issuer in [BACKEND.to_string(), format!("{BACKEND}/")] {
            let mut c = claims();
            c["iss"] = json!(issuer);
            assert!(service_account_hint(&jwt(c, "JWT"), BACKEND, None).is_none());
        }
        for typ in ["rise-access+jwt", "rise-identity+jwt", "rise-session+jwt"] {
            assert!(service_account_hint(&jwt(claims(), typ), BACKEND, None).is_none());
        }
    }

    #[test]
    fn malformed_tokens_and_unusable_claims_have_no_hint() {
        for token in ["opaque", "a.b.c", "", "e30.e30.signature"] {
            assert!(service_account_hint(token, BACKEND, None).is_none());
        }
        for (key, value) in [
            ("aud", json!([])),
            ("aud", json!(null)),
            ("aud", json!(42)),
            ("aud", json!("")),
            ("iss", json!(null)),
            ("sub", json!("line\nforged log")),
        ] {
            let mut c = claims();
            c[key] = value;
            assert!(service_account_hint(&jwt(c, "JWT"), BACKEND, None).is_none());
        }
    }

    #[test]
    fn missing_subject_has_explicit_placeholder() {
        let mut c = claims();
        c.as_object_mut().unwrap().remove("sub");
        let hint = service_account_hint(&jwt(c, "JWT"), BACKEND, None).unwrap();
        assert!(hint.contains("--claim 'sub=<subject>'"));
        assert!(hint.contains("Replace any placeholders"));
    }

    #[test]
    fn shell_quoting_preserves_claims_as_literal_arguments() {
        let value = "sub=repo:org/it's $(printf unsafe) `printf unsafe` ; *";
        let result = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf '%s' {}", shell_quote(value)))
            .output()
            .unwrap();
        assert!(result.status.success());
        assert_eq!(String::from_utf8(result.stdout).unwrap(), value);
    }

    #[derive(Clone)]
    struct LogBuffer(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogBuffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn request_logs_only_auth_failures_and_preserves_response() {
        for (status, external, expected_hint) in [
            (StatusCode::FORBIDDEN, true, true),
            (StatusCode::UNAUTHORIZED, true, true),
            (StatusCode::OK, true, false),
            (StatusCode::BAD_REQUEST, true, false),
            (StatusCode::INTERNAL_SERVER_ERROR, true, false),
            (StatusCode::FORBIDDEN, false, false),
        ] {
            let app =
                axum::Router::new().fallback(move || async move { (status, "original response") });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let backend = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let mut c = claims();
            if !external {
                c["iss"] = json!(backend);
            }
            let log = LogBuffer(Arc::new(Mutex::new(Vec::new())));
            let writer = log.clone();
            let subscriber = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .finish();
            let response = reqwest::Client::new()
                .get(format!(
                    "{backend}/api/v1/projects/my%20project/deployments"
                ))
                .bearer_auth(jwt(c, "JWT"))
                .timeout(std::time::Duration::from_secs(5))
                .send_with_auth_hint(&backend)
                .with_subscriber(subscriber)
                .await
                .unwrap();
            assert_eq!(response.status(), status);
            assert_eq!(response.text().await.unwrap(), "original response");
            server.abort();
            let output = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
            assert_eq!(
                output.contains("rise service-account create"),
                expected_hint,
                "{status}: {output}"
            );
            if expected_hint {
                assert!(output.contains("--project 'my project'"));
            }
        }
    }
}
