//! Pure environment-variable merge + hashing helpers used by desired
//! computation. All `&self`-free and unit-testable without a daemon.

use sha2::{Digest, Sha256};

/// Merge env for one container in final precedence:
/// base (plain + secret) → system env → per-container overrides. Later writes
/// win on key conflict.
pub(crate) fn merge_container_env(
    base_env: &[(String, String)],
    system_env: &[(String, String)],
    injected_hosts: &[(String, String)],
    spec: &crate::server::deployment::models::ContainerSpec,
    env_name: Option<&str>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = base_env.to_vec();
    for (k, v) in system_env {
        upsert_env(&mut env, k, v);
    }
    // Cross-container service-discovery hosts (`RISE_CONTAINER_HOST__<NAME>`).
    // Only added when not already present, so user globals win; per-container
    // `env_overrides` below run last and can shadow them too — matching the K8s
    // precedence in `webhook.rs`.
    for (k, v) in injected_hosts {
        if !env.iter().any(|(ek, _)| ek == k) {
            env.push((k.clone(), v.clone()));
        }
    }
    for over in &spec.env_overrides {
        // Per-container secret overrides are rejected at request time.
        if over.is_secret {
            continue;
        }
        if let Some(ref target_env) = over.for_environment {
            if env_name != Some(target_env.as_str()) {
                continue;
            }
        }
        upsert_env(&mut env, &over.key, &over.value);
    }
    env
}

pub(crate) fn upsert_env(env: &mut Vec<(String, String)>, key: &str, value: &str) {
    if let Some(existing) = env.iter_mut().find(|(k, _)| k == key) {
        existing.1 = value.to_string();
    } else {
        env.push((key.to_string(), value.to_string()));
    }
}

/// Stable sha256 of a merged env vector, used as the drift label. Hashes the
/// *entire* set (plain + system/RISE_* + secret) over a deterministically
/// key-sorted copy with length-prefixed key/value framing, so reordering can't
/// change the digest while any add/edit/delete of any variable does. Editing or
/// deleting any env var therefore changes the `env-hash` label and forces the
/// reconciler to recreate the container.
pub fn hash_env(env: &[(String, String)]) -> String {
    let mut sorted: Vec<&(String, String)> = env.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let mut hasher = Sha256::new();
    for (k, v) in sorted {
        hasher.update((k.len() as u64).to_le_bytes());
        hasher.update(k.as_bytes());
        hasher.update((v.len() as u64).to_le_bytes());
        hasher.update(v.as_bytes());
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_env_precedence() {
        use crate::server::deployment::models::{ContainerSpec, EnvOverride};
        let base = vec![("FOO".to_string(), "base".to_string())];
        let system = vec![
            ("FOO".to_string(), "system".to_string()),
            ("RISE_APP_URL".to_string(), "url".to_string()),
        ];
        let spec = ContainerSpec {
            name: "app".to_string(),
            image: None,
            port: Some(8080),
            replicas: None,
            cpu: None,
            memory: None,
            env_overrides: vec![EnvOverride {
                key: "FOO".to_string(),
                value: "override".to_string(),
                is_secret: false,
                is_protected: None,
                source: None,
                for_environment: None,
            }],
            health_check: None,
        };
        let merged = merge_container_env(&base, &system, &[], &spec, None);
        let foo = merged.iter().find(|(k, _)| k == "FOO").unwrap();
        assert_eq!(foo.1, "override");
        assert!(merged
            .iter()
            .any(|(k, v)| k == "RISE_APP_URL" && v == "url"));
    }

    #[test]
    fn merge_env_injects_container_hosts_unless_user_set() {
        use crate::server::deployment::models::{ContainerSpec, EnvOverride};
        // A user global and a per-container override both targeting the same
        // discovery key must win over the auto-injected sibling host.
        let base = vec![(
            "RISE_CONTAINER_HOST__API".to_string(),
            "user-global:1".to_string(),
        )];
        // Injected hosts use the GROUP-scoped, deployment-id-FREE alias
        // (`{prefix}_{project}_{group}_{container}`).
        let injected = vec![
            (
                "RISE_CONTAINER_HOST__API".to_string(),
                "rise_myapp_default_api:8080".to_string(),
            ),
            (
                "RISE_CONTAINER_HOST__REDIS".to_string(),
                "rise_myapp_default_redis:6379".to_string(),
            ),
        ];
        let spec = ContainerSpec {
            name: "web".to_string(),
            image: None,
            port: Some(8080),
            replicas: None,
            cpu: None,
            memory: None,
            env_overrides: vec![EnvOverride {
                key: "RISE_CONTAINER_HOST__REDIS".to_string(),
                value: "override-redis:6379".to_string(),
                is_secret: false,
                is_protected: None,
                source: None,
                for_environment: None,
            }],
            health_check: None,
        };
        let merged = merge_container_env(&base, &[], &injected, &spec, None);
        // User global wins; injected value is not appended a second time.
        let api: Vec<_> = merged
            .iter()
            .filter(|(k, _)| k == "RISE_CONTAINER_HOST__API")
            .collect();
        assert_eq!(api.len(), 1);
        assert_eq!(api[0].1, "user-global:1");
        // Per-container override shadows the injected sibling host.
        let redis = merged
            .iter()
            .find(|(k, _)| k == "RISE_CONTAINER_HOST__REDIS")
            .unwrap();
        assert_eq!(redis.1, "override-redis:6379");
    }

    #[test]
    fn merge_env_skips_non_matching_environment_override() {
        use crate::server::deployment::models::{ContainerSpec, EnvOverride};
        let spec = ContainerSpec {
            name: "app".to_string(),
            image: None,
            port: None,
            replicas: None,
            cpu: None,
            memory: None,
            env_overrides: vec![EnvOverride {
                key: "ONLY_PROD".to_string(),
                value: "1".to_string(),
                is_secret: false,
                is_protected: None,
                source: None,
                for_environment: Some("production".to_string()),
            }],
            health_check: None,
        };
        let merged = merge_container_env(&[], &[], &[], &spec, Some("staging"));
        assert!(!merged.iter().any(|(k, _)| k == "ONLY_PROD"));
        let merged_prod = merge_container_env(&[], &[], &[], &spec, Some("production"));
        assert!(merged_prod.iter().any(|(k, _)| k == "ONLY_PROD"));
    }

    #[test]
    fn hash_env_is_order_independent_but_value_sensitive() {
        let a = vec![
            ("A".to_string(), "1".to_string()),
            ("B".to_string(), "2".to_string()),
        ];
        let b = vec![
            ("B".to_string(), "2".to_string()),
            ("A".to_string(), "1".to_string()),
        ];
        // Reordering the same set yields the same hash.
        assert_eq!(hash_env(&a), hash_env(&b));
        // Changing a value changes the hash.
        let changed = vec![
            ("A".to_string(), "1".to_string()),
            ("B".to_string(), "3".to_string()),
        ];
        assert_ne!(hash_env(&a), hash_env(&changed));
        // Deleting a var changes the hash.
        let deleted = vec![("A".to_string(), "1".to_string())];
        assert_ne!(hash_env(&a), hash_env(&deleted));
        // Adding a plain var changes the hash (the core drift bug this fixes).
        let mut added = a.clone();
        added.push(("C".to_string(), "3".to_string()));
        assert_ne!(hash_env(&a), hash_env(&added));
    }

    #[test]
    fn hash_env_avoids_delimiter_collisions() {
        // Length-prefixed framing means `{A:"B", : "C"}`-style splits can't
        // collide with `{A:"BC"}`-style merges.
        let split = vec![
            ("A".to_string(), "B".to_string()),
            ("C".to_string(), "D".to_string()),
        ];
        let merged = vec![("A".to_string(), "BCD".to_string())];
        assert_ne!(hash_env(&split), hash_env(&merged));
    }
}
