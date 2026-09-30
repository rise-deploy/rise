#![cfg(feature = "cli")]

use axum::{extract::State, http::HeaderMap, routing::get, Json, Router};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs;
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct Profiles {
    home: TempDir,
}

impl Profiles {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".config/rise/profiles")).unwrap();
        Self { home }
    }

    fn save(&self, name: &str, url: &str, token: Option<&str>) {
        let dir = self.home.path().join(".config/rise");
        let path = if name == "default" {
            dir.join("config.json")
        } else {
            dir.join("profiles").join(format!("{name}.json"))
        };
        fs::write(
            path,
            json!({ "backend_url": url, "token": token }).to_string(),
        )
        .unwrap();
    }

    fn credential(&self, url: &str, token: &str) -> std::path::PathBuf {
        let url = url.trim_end_matches('/');
        let key: String = Sha256::digest(url.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let dir = self.home.path().join(".config/rise/credentials");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{key}.json"));
        fs::write(
            &path,
            json!({"backend_url": url, "token": token}).to_string(),
        )
        .unwrap();
        path
    }

    fn select_default(&self, name: &str) {
        fs::write(self.home.path().join(".config/rise/default-profile"), name).unwrap();
    }

    fn project(&self, contents: &str) {
        fs::write(self.home.path().join("rise.toml"), contents).unwrap();
    }

    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rise"));
        for (key, _) in std::env::vars() {
            if key.starts_with("RISE_")
                || key.starts_with("ACTIONS_")
                || key.to_lowercase().ends_with("_proxy")
            {
                command.env_remove(key);
            }
        }
        command
            .args(args)
            .current_dir(self.home.path())
            .env("HOME", self.home.path())
            .env("NO_COLOR", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in env {
            command.env(key, value);
        }
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                let output = child.wait_with_output().unwrap();
                panic!("CLI timed out: {}", String::from_utf8_lossy(&output.stderr));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        child.wait_with_output().unwrap()
    }
}

struct Backend {
    url: String,
    tokens: Arc<Mutex<Vec<String>>>,
    server: tokio::task::JoinHandle<()>,
}

impl Backend {
    async fn start() -> Self {
        async fn projects(
            State(tokens): State<Arc<Mutex<Vec<String>>>>,
            headers: HeaderMap,
        ) -> Json<serde_json::Value> {
            tokens.lock().unwrap().push(
                headers
                    .get("authorization")
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string(),
            );
            Json(json!([]))
        }
        let tokens = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/api/v1/projects", get(projects))
            .route(
                "/api/v1/version",
                get(|| async {
                    Json(json!({"version": env!("CARGO_PKG_VERSION"), "repository": "test"}))
                }),
            )
            .route(
                "/api/v1/auth/authorize",
                axum::routing::post(|| async {
                    (
                        axum::http::StatusCode::BAD_REQUEST,
                        "test login backend reached",
                    )
                }),
            )
            .with_state(tokens.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            url,
            tokens,
            server,
        }
    }

    fn assert_token(&self, token: &str) {
        assert_eq!(
            self.tokens.lock().unwrap().last().unwrap(),
            &format!("Bearer {token}")
        );
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn success(output: Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn url_credentials_work_without_alias_and_ignore_unrelated_default_token() {
    let backend = Backend::start().await;
    let profiles = Profiles::new();
    profiles.save("work", "https://other.example.com", Some("wrong-token"));
    profiles.select_default("work");
    profiles.credential(&backend.url, "url-token");
    success(profiles.run(
        &["project", "list"],
        &[("RISE_URL", &format!("{}///", backend.url))],
    ));
    backend.assert_token("url-token");
}

#[tokio::test(flavor = "multi_thread")]
async fn aliases_share_url_credentials_and_canonical_token_wins() {
    let backend = Backend::start().await;
    let profiles = Profiles::new();
    profiles.save("work", &backend.url, Some("stale-work-token"));
    profiles.save(
        "personal",
        &format!("{}/", backend.url),
        Some("stale-personal-token"),
    );
    profiles.credential(&backend.url, "shared-token");
    for profile in ["work", "personal"] {
        success(profiles.run(&["project", "list", "--profile", profile], &[]));
        backend.assert_token("shared-token");
    }
    success(profiles.run(&["project", "list"], &[("RISE_URL", &backend.url)]));
    backend.assert_token("shared-token");
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_url_does_not_reuse_selected_profile_credentials() {
    let backend = Backend::start().await;
    let profiles = Profiles::new();
    profiles.save("work", "https://other.example.com", Some("wrong-token"));
    profiles.select_default("work");
    for args in [
        vec!["project", "list"],
        vec!["project", "list", "--profile", "work"],
    ] {
        let output = profiles.run(&args, &[("RISE_URL", &backend.url)]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("Not authenticated"));
        assert!(backend.tokens.lock().unwrap().is_empty());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn existing_profile_tokens_are_reused_only_for_their_url() {
    let backend = Backend::start().await;
    let profiles = Profiles::new();
    profiles.save("default", &backend.url, Some("existing-token"));
    profiles.save("work", &format!("{}/", backend.url), Some("existing-token"));
    success(profiles.run(&["project", "list"], &[("RISE_URL", &backend.url)]));
    backend.assert_token("existing-token");
}

#[tokio::test(flavor = "multi_thread")]
async fn conflicting_existing_tokens_require_login_and_login_can_resolve_target() {
    let backend = Backend::start().await;
    let profiles = Profiles::new();
    profiles.save("personal", &backend.url, Some("personal-token"));
    profiles.save("work", &backend.url, Some("work-token"));
    let output = profiles.run(&["project", "list"], &[("RISE_URL", &backend.url)]);
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("different saved tokens"), "{error}");
    assert!(error.contains("personal, work"), "{error}");
    let output = profiles.run(
        &["login", "--url", &backend.url, "--device"],
        &[("RISE_URL", "https://other.example.com")],
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("test login backend reached"));
}

#[tokio::test(flavor = "multi_thread")]
async fn default_target_url_and_profile_override_persisted_default() {
    let backend = Backend::start().await;
    let profiles = Profiles::new();
    profiles.save("other", "https://other.example.com", None);
    profiles.select_default("other");
    profiles.save("work", &backend.url, None);
    profiles.credential(&backend.url, "target-token");
    for target in [
        format!("url = '{}'", backend.url),
        "profile = 'work'".to_string(),
    ] {
        profiles.project(&format!("[targets.default]\n{target}\n"));
        success(profiles.run(&["project", "list"], &[]));
        backend.assert_token("target-token");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_overrides_skip_project_target_and_profile_flag_beats_environment() {
    let backend = Backend::start().await;
    let profiles = Profiles::new();
    profiles.save("work", &backend.url, None);
    profiles.credential(&backend.url, "override-token");
    profiles.project("[targets.default]\nprofile = 'missing'\nurl = 'invalid'\n");
    success(profiles.run(&["project", "list"], &[("RISE_URL", &backend.url)]));
    success(profiles.run(&["project", "list"], &[("RISE_PROFILE", "work")]));
    success(profiles.run(
        &["project", "list", "--profile", "work"],
        &[("RISE_PROFILE", "missing")],
    ));
    backend.assert_token("override-token");
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_targets_preserve_persisted_default() {
    let backend = Backend::start().await;
    let profiles = Profiles::new();
    profiles.save("work", &backend.url, None);
    profiles.credential(&backend.url, "default-token");
    profiles.select_default("work");
    for contents in ["", "[targets.staging]\nprofile = 'missing'\n"] {
        profiles.project(contents);
        success(profiles.run(&["project", "list"], &[]));
        backend.assert_token("default-token");
    }
}

#[test]
fn missing_target_profile_errors_before_network_access() {
    let profiles = Profiles::new();
    profiles.project("[targets.default]\nprofile = 'missing'\n");
    for args in [vec!["project", "list"], vec!["login", "--device"]] {
        let output = profiles.run(&args, &[]);
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("Profile 'missing' does not exist")
        );
    }
}

#[test]
fn project_target_uses_command_path_and_dot_rise_toml() {
    let profiles = Profiles::new();
    profiles.project("[targets.default]\nprofile = 'wrong-directory'\n");
    fs::create_dir(profiles.home.path().join("app")).unwrap();
    fs::write(
        profiles.home.path().join("app/.rise.toml"),
        "[targets.default]\nprofile = 'project-directory'\n",
    )
    .unwrap();
    for args in [
        vec!["project", "show", "--path", "app"],
        vec!["deploy", "app"],
    ] {
        let output = profiles.run(&args, &[]);
        assert!(String::from_utf8_lossy(&output.stderr)
            .contains("Profile 'project-directory' does not exist"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn removing_alias_keeps_credential_including_a_profile_local_token() {
    let backend = Backend::start().await;
    let profiles = Profiles::new();
    profiles.save("work", &backend.url, Some("kept-token"));
    success(profiles.run(&["profile", "remove", "work"], &[]));
    success(profiles.run(&["project", "list"], &[("RISE_URL", &backend.url)]));
    backend.assert_token("kept-token");
}

#[test]
fn profile_management_ignores_project_targets() {
    let profiles = Profiles::new();
    profiles.project("[targets.default]\nprofile = 'missing'\nurl = 'invalid'\n");
    profiles.save("work", "https://rise.example.com", None);
    success(profiles.run(&["profile", "use", "work"], &[]));
    success(profiles.run(&["profile", "list"], &[]));
}
