//! Workload identity token exchange.
//!
//! Lets a deployed app exchange its per-deployment bootstrap credential for a
//! short-lived, Rise-signed OIDC JWT describing the Rise identity (project +
//! environment), for federating to external systems (AWS STS, GCP WIF, ...).

pub mod handlers;
pub mod models;
pub mod routes;

/// Build the subject claim for a workload identity token.
///
/// Fixed and environment-aware: `rise:proj:<project>:env:<environment>`.
/// `_none` is used literally when the deployment has no environment.
pub fn workload_subject(project: &str, environment: Option<&str>) -> String {
    format!(
        "rise:proj:{}:env:{}",
        project,
        environment.unwrap_or("_none")
    )
}

#[cfg(test)]
mod tests {
    use super::workload_subject;

    #[test]
    fn workload_subject_with_environment() {
        assert_eq!(
            workload_subject("myapp", Some("prod")),
            "rise:proj:myapp:env:prod"
        );
    }

    #[test]
    fn workload_subject_without_environment() {
        assert_eq!(workload_subject("myapp", None), "rise:proj:myapp:env:_none");
    }
}
