#![cfg(feature = "cli")]

use serde_json::json;
use std::fs;
use std::process::{Command, Output};
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

    fn save(&self, name: &str, url: Option<&str>) {
        let dir = self.home.path().join(".config/rise");
        let path = if name == "default" {
            dir.join("config.json")
        } else {
            dir.join("profiles").join(format!("{name}.json"))
        };
        fs::write(path, json!({ "backend_url": url }).to_string()).unwrap();
    }

    fn select_default(&self, name: &str) {
        fs::write(self.home.path().join(".config/rise/default-profile"), name).unwrap();
    }

    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rise"));
        command
            .args(args)
            .env("HOME", self.home.path())
            .env_remove("RISE_PROFILE")
            .env_remove("RISE_URL")
            .env("NO_COLOR", "1");
        for (key, value) in env {
            command.env(key, value);
        }
        command.output().unwrap()
    }

    fn assert_active(&self, args: &[&str], env: &[(&str, &str)], name: &str) {
        let output = self.run(args, env);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(&format!("Active profile: {name}\n")),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

#[test]
fn url_selects_named_profile_over_persisted_default() {
    let profiles = Profiles::new();
    profiles.save("personal", Some("https://personal.example.com"));
    profiles.save("work", Some("https://work.example.com/"));
    profiles.select_default("personal");
    profiles.assert_active(
        &["profile", "list"],
        &[("RISE_URL", "https://work.example.com///")],
        "work",
    );
    profiles.assert_active(&["profile", "list"], &[], "personal");
}

#[test]
fn url_selects_default_profile_over_persisted_default() {
    let profiles = Profiles::new();
    profiles.save("default", Some("https://personal.example.com"));
    profiles.save("work", Some("https://work.example.com"));
    profiles.select_default("work");
    profiles.assert_active(
        &["profile", "list"],
        &[("RISE_URL", "https://personal.example.com/")],
        "default",
    );
}

#[test]
fn unmatched_url_uses_persisted_default() {
    let profiles = Profiles::new();
    profiles.save("work", Some("https://work.example.com"));
    profiles.select_default("work");
    profiles.assert_active(
        &["profile", "list"],
        &[("RISE_URL", "https://unknown.example.com")],
        "work",
    );
}

#[test]
fn profiles_without_saved_urls_do_not_match_implicit_localhost() {
    let profiles = Profiles::new();
    profiles.save("empty", None);
    profiles.save("local", Some("http://localhost:3000"));
    profiles.assert_active(
        &["profile", "list"],
        &[("RISE_URL", "http://localhost:3000")],
        "local",
    );
}

#[test]
fn multiple_url_matches_error_including_default_profile() {
    let profiles = Profiles::new();
    for name in ["default", "work", "personal"] {
        profiles.save(name, Some("https://rise.example.com/"));
    }
    profiles.select_default("work");
    let output = profiles.run(
        &["profile", "list"],
        &[("RISE_URL", "https://rise.example.com")],
    );
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("Multiple profiles match backend URL"),
        "{error}"
    );
    assert!(error.contains("default, personal, work"), "{error}");
    assert!(error.contains("--profile or RISE_PROFILE"), "{error}");
}

#[test]
fn explicit_profiles_bypass_ambiguous_url_matching() {
    let profiles = Profiles::new();
    for name in ["default", "work", "personal"] {
        profiles.save(name, Some("https://rise.example.com"));
    }
    let env = [
        ("RISE_URL", "https://rise.example.com"),
        ("RISE_PROFILE", "work"),
    ];
    profiles.assert_active(&["profile", "list"], &env, "work");
    profiles.assert_active(
        &["profile", "list", "--profile", "personal"],
        &env,
        "personal",
    );
    profiles.assert_active(
        &["profile", "list", "--profile", "default"],
        &env,
        "default",
    );
    profiles.assert_active(
        &["profile", "list"],
        &[
            ("RISE_URL", "https://rise.example.com"),
            ("RISE_PROFILE", "default"),
        ],
        "default",
    );
}

#[test]
fn login_url_takes_priority_over_environment_url_for_matching() {
    let profiles = Profiles::new();
    profiles.save("other", Some("https://other.example.com"));
    profiles.save("personal", Some("https://rise.example.com"));
    profiles.save("work", Some("https://rise.example.com/"));
    let output = profiles.run(
        &["login", "--url", "https://rise.example.com///", "--device"],
        &[("RISE_URL", "https://other.example.com")],
    );
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("Multiple profiles match backend URL 'https://rise.example.com'"),
        "{error}"
    );
    assert!(error.contains("personal, work"), "{error}");
}
