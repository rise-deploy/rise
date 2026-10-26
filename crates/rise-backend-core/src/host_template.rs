//! The shape rule that keeps every project's ingress hosts its own.
//!
//! A preview host is rendered from the staging or environment template, e.g.
//! `{deployment_group}--{project_name}.apps.example.com`. If those templates
//! joined the project to its group with an ambiguous separator, one project's
//! preview host could equal another project's: with
//! `{project_name}-{deployment_group}`, project `a` group `b-c` and project
//! `a-b` group `c` both render `a-b-c`.
//!
//! The rule makes the project recoverable from any host:
//!
//! - Project and environment names never contain `--`; normalized group names
//!   may. No name contains a `.`.
//! - The production template contains no `--`, so no production host does.
//! - The staging and environment templates join `{project_name}` to their
//!   other placeholder with exactly `--`, contain no other `--`, and bound
//!   `{project_name}` on its far side with a `.` or the template's end. The
//!   one label of a preview host that contains `--` therefore starts (or ends)
//!   with the whole project name, up to its first (or last) `--`.
//! - Both templates put `{project_name}` on the same side of the `--`, so a
//!   staging host and an environment host that are equal belong to the same
//!   project, where `DeploymentUrlBuilder::host_conflicts_with_other_env`
//!   resolves them.

/// Joins `{project_name}` to `{deployment_group}` / `{environment}` in
/// preview hostnames. Project and environment names may not contain it.
pub const HOST_SEPARATOR: &str = "--";

/// The staging template used when none is configured: the production template
/// with `{project_name}--{deployment_group}` in place of `{project_name}`.
pub fn synthesized_staging_template(production: &str) -> String {
    production.replacen("{project_name}", "{project_name}--{deployment_group}", 1)
}

/// Which side of the `--` a preview template puts `{project_name}` on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectSide {
    /// `{project_name}--{other}`
    Left,
    /// `{other}--{project_name}`
    Right,
}

/// Check the three ingress templates against the shape rule in the module
/// docs. `staging` is `None` when the operator configured none, in which case
/// the [`synthesized_staging_template`] is checked instead. The error names
/// the offending template and says what to change.
pub fn check_ingress_templates(
    production: &str,
    staging: Option<&str>,
    environment: Option<&str>,
) -> Result<(), String> {
    if production.contains(HOST_SEPARATOR) {
        return Err(format!(
            "production_ingress_url_template must not contain '{HOST_SEPARATOR}', which \
             separates the project from its group or environment in preview hosts. \
             Got: '{production}'"
        ));
    }

    let (staging_field, staging) = match staging {
        Some(staging) => ("staging_ingress_url_template", staging.to_string()),
        None => (
            "the staging template derived from production_ingress_url_template (set \
             staging_ingress_url_template to choose another)",
            synthesized_staging_template(production),
        ),
    };
    let staging_side = preview_template_side(&staging, "{deployment_group}")
        .map_err(|reason| format!("{staging_field} {reason}. Got: '{staging}'"))?;

    if let Some(environment) = environment {
        let environment_side =
            preview_template_side(environment, "{environment}").map_err(|reason| {
                format!("environment_ingress_url_template {reason}. Got: '{environment}'")
            })?;
        if environment_side != staging_side {
            return Err(format!(
                "environment_ingress_url_template and {staging_field} must put \
                 {{project_name}} on the same side of '{HOST_SEPARATOR}'. \
                 Got: '{environment}' and '{staging}'"
            ));
        }
    }
    Ok(())
}

/// Check one staging or environment template and return which side of the
/// `--` it puts `{project_name}` on. The error completes the sentence
/// "<template> ...".
fn preview_template_side(template: &str, other: &str) -> Result<ProjectSide, String> {
    const PROJECT: &str = "{project_name}";
    for placeholder in [PROJECT, other] {
        if template.matches(placeholder).count() != 1 {
            return Err(format!("must contain '{placeholder}' exactly once"));
        }
    }

    let left = format!("{PROJECT}{HOST_SEPARATOR}{other}");
    let right = format!("{other}{HOST_SEPARATOR}{PROJECT}");
    let (side, pair) = if template.contains(&left) {
        (ProjectSide::Left, left)
    } else if template.contains(&right) {
        (ProjectSide::Right, right)
    } else {
        return Err(format!(
            "must join '{PROJECT}' and '{other}' with '{HOST_SEPARATOR}', as in \
             '{right}.apps.example.com'"
        ));
    };

    let (before, after) = template
        .split_once(&pair)
        .expect("the pair was just found in the template");
    if before.contains(HOST_SEPARATOR) || after.contains(HOST_SEPARATOR) {
        return Err(format!(
            "must contain '{HOST_SEPARATOR}' only between '{PROJECT}' and '{other}'"
        ));
    }
    let bounded = match side {
        ProjectSide::Left => before.is_empty() || before.ends_with('.'),
        ProjectSide::Right => after.is_empty() || after.starts_with('.'),
    };
    if !bounded {
        return Err(format!(
            "must have '{PROJECT}' start or end its DNS label: put a '.' (or nothing) \
             on its other side"
        ));
    }
    Ok(side)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_templates_that_keep_the_project_recoverable() {
        for (production, staging, environment) in [
            // The shipped defaults.
            (
                "{project_name}.apps.example.com",
                Some("{deployment_group}--{project_name}.apps.example.com"),
                Some("{environment}--{project_name}.apps.example.com"),
            ),
            (
                "{project_name}.apps.example.com",
                Some("{project_name}--{deployment_group}.preview.example.com"),
                Some("{project_name}--{environment}.preview.example.com"),
            ),
            // Different suffixes per template.
            (
                "{project_name}.rise.local",
                Some("{deployment_group}--{project_name}.preview.rise.local"),
                Some("{environment}--{project_name}.env.rise.local"),
            ),
            // No staging template: derived from production.
            ("{project_name}.apps.example.com", None, None),
            (
                "{project_name}.apps.example.com",
                None,
                Some("{project_name}--{environment}.apps.example.com"),
            ),
            // A label prefix before a left-hand project.
            (
                "{project_name}.apps.example.com",
                Some("pr.{project_name}--{deployment_group}.example.com"),
                None,
            ),
        ] {
            assert_eq!(
                check_ingress_templates(production, staging, environment),
                Ok(()),
                "{production} / {staging:?} / {environment:?}"
            );
        }
    }

    #[test]
    fn rejects_templates_that_let_projects_share_a_host() {
        for (production, staging, environment, why) in [
            (
                "{project_name}.apps.example.com",
                Some("{project_name}-{deployment_group}.preview.example.com"),
                None,
                "single hyphen: `a`+`b-c` and `a-b`+`c` both give `a-b-c`",
            ),
            (
                "{project_name}--x.apps.example.com",
                None,
                None,
                "production host containing the separator",
            ),
            (
                "{project_name}.apps.example.com",
                Some("{deployment_group}--{project_name}-pr.apps.example.com"),
                None,
                "project not bounded by a dot",
            ),
            (
                "{project_name}.apps.example.com",
                Some("{deployment_group}--{project_name}.x--y.example.com"),
                None,
                "a second separator",
            ),
            (
                "{project_name}.apps.example.com",
                Some("{deployment_group}--{project_name}.apps.example.com"),
                Some("{project_name}--{environment}.apps.example.com"),
                "project on opposite sides: env `a`+`b` vs group `b`+`a`",
            ),
            (
                "app-{project_name}.example.com",
                None,
                None,
                "derived staging template with an unbounded project",
            ),
        ] {
            assert!(
                check_ingress_templates(production, staging, environment).is_err(),
                "should be rejected ({why}): {production} / {staging:?} / {environment:?}"
            );
        }
    }

    #[test]
    fn synthesizes_the_staging_template_from_production() {
        assert_eq!(
            synthesized_staging_template("{project_name}.apps.example.com"),
            "{project_name}--{deployment_group}.apps.example.com"
        );
    }
}
