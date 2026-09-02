use serde::{Deserialize, Serialize};

/// Deserialize a field of type `Option<Option<T>>` so that a present JSON
/// `null` is distinguishable from an absent field.
///
/// `#[serde(default)]` alone cannot make that distinction: serde's blanket
/// `Deserialize` impl for `Option<U>` treats a `null` value as `None` before
/// this field's own type (`Option<Option<T>>`, so `U = Option<T>`) ever gets a
/// say — so a present `null` and an absent field both end up as the outer
/// `None`. Deserializing the inner `Option<T>` directly (skipping that outer
/// layer, which `#[serde(default)]` already covers for the absent case) and
/// wrapping the result in `Some` fixes that: `null` becomes `Some(None)`
/// ("clear it"), a value becomes `Some(Some(v))` ("set it"), and the field
/// being absent entirely is handled by `#[serde(default)]` before this
/// function is even called, yielding the outer `None` ("leave unchanged").
fn deserialize_double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

#[derive(Debug, Deserialize)]
pub struct CreateEnvironmentRequest {
    pub name: String,
    #[serde(default)]
    pub primary_deployment_group: Option<String>,
    #[serde(default)]
    pub is_production: bool,
    #[serde(default = "default_color")]
    pub color: String,
    /// Caps `expires_at` of deployments created into a non-primary group.
    /// `Nd|Nh|Nm`, e.g. `"7d"`. Absent means no cap.
    #[serde(default)]
    pub max_deployment_expiration: Option<String>,
}

fn default_color() -> String {
    "green".to_string()
}

/// Per-environment deployment constraints (admin-settable)
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct EnvironmentDeploymentConstraints {
    pub min_replicas: Option<u32>,
    pub max_replicas: Option<u32>,
    pub min_cpu: Option<String>,
    pub max_cpu: Option<String>,
    pub min_memory: Option<String>,
    pub max_memory: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateEnvironmentRequest {
    pub name: Option<String>,
    /// Use `Some(None)` to unset, `Some(Some(group))` to set, `None` to leave unchanged.
    #[serde(default, deserialize_with = "deserialize_double_option")]
    pub primary_deployment_group: Option<Option<String>>,
    pub is_production: Option<bool>,
    pub color: Option<String>,
    /// Per-environment deployment constraints (admin-only)
    #[serde(default)]
    pub deployment_constraints: Option<EnvironmentDeploymentConstraints>,
    /// Use `Some(None)` to clear the cap, `Some(Some(duration))` to set it,
    /// `None` to leave unchanged.
    #[serde(default, deserialize_with = "deserialize_double_option")]
    pub max_deployment_expiration: Option<Option<String>>,
}

#[derive(Debug, Serialize)]
pub struct EnvironmentResponse {
    pub name: String,
    pub primary_deployment_group: Option<String>,
    pub is_production: bool,
    pub color: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deployment_constraints: Option<EnvironmentDeploymentConstraints>,
    pub max_deployment_expiration: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl From<crate::db::models::Environment> for EnvironmentResponse {
    fn from(env: crate::db::models::Environment) -> Self {
        // Only include constraints if at least one field is set
        let deployment_constraints = if env.min_replicas.is_some()
            || env.max_replicas.is_some()
            || env.min_cpu.is_some()
            || env.max_cpu.is_some()
            || env.min_memory.is_some()
            || env.max_memory.is_some()
        {
            Some(EnvironmentDeploymentConstraints {
                min_replicas: env.min_replicas.and_then(|v| u32::try_from(v).ok()),
                max_replicas: env.max_replicas.and_then(|v| u32::try_from(v).ok()),
                min_cpu: env.min_cpu,
                max_cpu: env.max_cpu,
                min_memory: env.min_memory,
                max_memory: env.max_memory,
            })
        } else {
            None
        };

        Self {
            name: env.name,
            primary_deployment_group: env.primary_deployment_group,
            is_production: env.is_production,
            color: env.color,
            deployment_constraints,
            max_deployment_expiration: env.max_deployment_expiration,
            created_at: env.created_at.to_rfc3339(),
            updated_at: env.updated_at.to_rfc3339(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_request_double_option_fields_distinguish_absent_null_and_value() {
        // Field absent entirely: `#[serde(default)]` fires -> outer `None`,
        // meaning "leave unchanged".
        let req: UpdateEnvironmentRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(req.primary_deployment_group, None);
        assert_eq!(req.max_deployment_expiration, None);

        // Field present and explicitly `null`: `Some(None)`, meaning "clear it".
        let req: UpdateEnvironmentRequest = serde_json::from_str(
            r#"{"primary_deployment_group": null, "max_deployment_expiration": null}"#,
        )
        .unwrap();
        assert_eq!(req.primary_deployment_group, Some(None));
        assert_eq!(req.max_deployment_expiration, Some(None));

        // Field present with a value: `Some(Some(value))`, meaning "set it".
        let req: UpdateEnvironmentRequest = serde_json::from_str(
            r#"{"primary_deployment_group": "mr/123", "max_deployment_expiration": "7d"}"#,
        )
        .unwrap();
        assert_eq!(
            req.primary_deployment_group,
            Some(Some("mr/123".to_string()))
        );
        assert_eq!(req.max_deployment_expiration, Some(Some("7d".to_string())));
    }
}
