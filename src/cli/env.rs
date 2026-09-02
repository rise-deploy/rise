use anyhow::{Context, Result};
use comfy_table::{modifiers::UTF8_ROUND_CORNERS, presets::UTF8_FULL, Attribute, Cell, Table};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
struct EnvVarResponse {
    key: String,
    value: String, // Will be masked ("••••••••") for protected secrets
    is_secret: bool,
    is_protected: bool,
    #[serde(default)]
    environment: Option<String>,
    source: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EnvVarsResponse {
    env_vars: Vec<EnvVarResponse>,
}

#[derive(Debug, Serialize)]
struct SetEnvVarRequest {
    value: String,
    #[serde(default)]
    is_secret: bool,
    #[serde(default)]
    is_protected: bool,
}

/// Return a supplied value or prompt for one when it is omitted.
pub fn resolve_env_value(value: Option<&str>, is_secret: bool) -> Result<String> {
    if let Some(value) = value {
        return Ok(value.to_string());
    }

    if is_secret {
        return rpassword::prompt_password("Value: ")
            .context("Failed to read environment variable value");
    }

    read_plain_env_value(&mut std::io::stdin().lock(), &mut std::io::stderr())
}

fn read_plain_env_value(reader: &mut impl BufRead, writer: &mut impl Write) -> Result<String> {
    write!(writer, "Value: ").context("Failed to write environment variable prompt")?;
    writer
        .flush()
        .context("Failed to write environment variable prompt")?;

    let mut value = String::new();
    reader
        .read_line(&mut value)
        .context("Failed to read environment variable value")?;
    value.truncate(value.trim_end_matches(['\r', '\n']).len());
    Ok(value)
}

/// Build a URL with an optional `?environment=` query parameter
fn env_url(backend_url: &str, project: &str, suffix: &str, environment: Option<&str>) -> String {
    let base = format!("{}/api/v1/projects/{}/env{}", backend_url, project, suffix);
    if let Some(env_name) = environment {
        format!("{}?environment={}", base, urlencoding::encode(env_name))
    } else {
        base
    }
}

/// Fetch environment variables from a project (internal helper)
async fn fetch_env_vars_response(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    project: &str,
    environment: Option<&str>,
) -> Result<EnvVarsResponse> {
    let url = env_url(backend_url, project, "", environment);

    let response = http_client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
        .context("Failed to fetch environment variables")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!(
            "Failed to fetch environment variables (status {}): {}",
            status,
            error_text
        );
    }

    let env_vars_response: EnvVarsResponse = response
        .json()
        .await
        .context("Failed to parse environment variables response")?;

    Ok(env_vars_response)
}

/// Fetch preview environment variables — the full set a deployment would receive.
///
/// Returns:
/// - Loadable vars (non-secret + unprotected secrets, with decrypted values)
/// - Protected keys (value masked, cannot be loaded locally)
pub async fn fetch_preview_env_vars(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    project: &str,
    deployment_group: &str,
    environment: Option<&str>,
) -> Result<(Vec<(String, String)>, Vec<String>)> {
    let mut url = format!(
        "{}/api/v1/projects/{}/env/preview?deployment_group={}",
        backend_url, project, deployment_group
    );
    if let Some(env_name) = environment {
        url.push_str(&format!("&environment={}", urlencoding::encode(env_name)));
    }

    let response = http_client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
        .context("Failed to fetch preview environment variables")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!(
            "Failed to fetch preview environment variables (status {}): {}",
            status,
            error_text
        );
    }

    let env_response: EnvVarsResponse = response
        .json()
        .await
        .context("Failed to parse preview environment variables response")?;

    let mut loadable_vars = Vec::new();
    let mut protected_keys = Vec::new();

    for var in env_response.env_vars {
        if var.is_protected {
            protected_keys.push(var.key);
        } else {
            loadable_vars.push((var.key, var.value));
        }
    }

    Ok((loadable_vars, protected_keys))
}

/// Set an environment variable for a project
#[allow(clippy::too_many_arguments)]
pub async fn set_env(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    project: &str,
    key: &str,
    value: &str,
    is_secret: bool,
    is_protected: bool,
    environment: Option<&str>,
) -> Result<()> {
    let url = env_url(backend_url, project, &format!("/{}", key), environment);

    let payload = SetEnvVarRequest {
        value: value.to_string(),
        is_secret,
        is_protected,
    };

    let response = http_client
        .put(&url)
        .header("Authorization", format!("Bearer {}", token))
        .json(&payload)
        .send()
        .await
        .context("Failed to set environment variable")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!(
            "Failed to set environment variable (status {}): {}",
            status,
            error_text
        );
    }

    let var_type = if is_secret {
        if is_protected {
            "protected secret"
        } else {
            "unprotected secret"
        }
    } else {
        "plain text"
    };
    if let Some(env_name) = environment {
        println!(
            "✓ Set {} variable '{}' for project '{}' (environment: {})",
            var_type, key, project, env_name
        );
    } else {
        println!(
            "✓ Set {} variable '{}' for project '{}'",
            var_type, key, project
        );
    }

    Ok(())
}

/// List environment variables for a project
pub async fn list_env(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    project: &str,
    environment: Option<&str>,
) -> Result<()> {
    let env_vars_response =
        fetch_env_vars_response(http_client, backend_url, token, project, environment).await?;

    if env_vars_response.env_vars.is_empty() {
        println!(
            "No environment variables configured for project '{}'",
            project
        );
        return Ok(());
    }

    // Check if any var has an environment label (only when showing all environments)
    let show_env_col = environment.is_none()
        && env_vars_response
            .env_vars
            .iter()
            .any(|v| v.environment.is_some());

    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS);

    if show_env_col {
        table.set_header(vec![
            Cell::new("KEY").add_attribute(Attribute::Bold),
            Cell::new("VALUE").add_attribute(Attribute::Bold),
            Cell::new("TYPE").add_attribute(Attribute::Bold),
            Cell::new("PROTECTED").add_attribute(Attribute::Bold),
            Cell::new("ENVIRONMENT").add_attribute(Attribute::Bold),
        ]);
    } else {
        table.set_header(vec![
            Cell::new("KEY").add_attribute(Attribute::Bold),
            Cell::new("VALUE").add_attribute(Attribute::Bold),
            Cell::new("TYPE").add_attribute(Attribute::Bold),
            Cell::new("PROTECTED").add_attribute(Attribute::Bold),
        ]);
    }

    for var in env_vars_response.env_vars {
        let var_type = if var.is_secret { "secret" } else { "plain" };
        let protected = if var.is_secret {
            if var.is_protected {
                "yes"
            } else {
                "no"
            }
        } else {
            "-"
        };
        let mut row = vec![
            Cell::new(&var.key),
            Cell::new(&var.value),
            Cell::new(var_type),
            Cell::new(protected),
        ];
        if show_env_col {
            row.push(Cell::new(var.environment.as_deref().unwrap_or("(global)")));
        }
        table.add_row(row);
    }

    println!("{}", table);
    println!("\nProject: {}", project);
    println!("Note: Secret values are always masked for security");

    Ok(())
}

/// Get the value of a specific environment variable
pub async fn get_env(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    project: &str,
    key: &str,
    environment: Option<&str>,
) -> Result<()> {
    // First, fetch the variable to check if it exists and get its metadata
    let env_vars_response =
        fetch_env_vars_response(http_client, backend_url, token, project, environment).await?;

    let env_var = env_vars_response
        .env_vars
        .into_iter()
        .find(|v| v.key == key)
        .ok_or_else(|| anyhow::anyhow!("Environment variable '{}' not found", key))?;

    // If it's a secret and protected, we can't get the value
    if env_var.is_secret && env_var.is_protected {
        anyhow::bail!(
            "Cannot retrieve value: '{}' is a protected secret.\n\
             To make it unprotected, update it with: rise env set {} <value> --secret --protected=false",
            key, key
        );
    }

    // If it's an unprotected secret, fetch the decrypted value
    if env_var.is_secret && !env_var.is_protected {
        let url = env_url(
            backend_url,
            project,
            &format!("/{}/value", key),
            environment,
        );

        let response = http_client
            .get(&url)
            .header("Authorization", format!("Bearer {}", token))
            .send()
            .await
            .context("Failed to get environment variable value")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            anyhow::bail!(
                "Failed to get environment variable value (status {}): {}",
                status,
                error_text
            );
        }

        #[derive(Debug, serde::Deserialize)]
        struct EnvVarValueResponse {
            value: String,
        }

        let value_response: EnvVarValueResponse = response
            .json()
            .await
            .context("Failed to parse environment variable value response")?;

        // Print just the value (useful for scripting)
        println!("{}", value_response.value);
    } else {
        // For non-secret variables, the value is already in the response
        println!("{}", env_var.value);
    }

    Ok(())
}

/// Delete an environment variable from a project
pub async fn unset_env(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    project: &str,
    key: &str,
    environment: Option<&str>,
) -> Result<()> {
    let url = env_url(backend_url, project, &format!("/{}", key), environment);

    let response = http_client
        .delete(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
        .context("Failed to delete environment variable")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!(
            "Failed to delete environment variable (status {}): {}",
            status,
            error_text
        );
    }

    println!("✓ Deleted variable '{}' from project '{}'", key, project);

    Ok(())
}

/// A parsed environment variable from a file or string
#[derive(Debug, Clone)]
pub struct ParsedEnvVar {
    pub key: String,
    pub value: String,
    pub is_secret: bool,
}

fn iter_env_file_lines(contents: &str) -> impl Iterator<Item = (usize, &str)> {
    contents.lines().enumerate().filter_map(|(line_num, line)| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            None
        } else {
            Some((line_num + 1, line))
        }
    })
}

/// Parse a single KEY=VALUE or KEY=secret:VALUE string
pub fn parse_env_string(s: &str) -> anyhow::Result<ParsedEnvVar> {
    let parts: Vec<&str> = s.splitn(2, '=').collect();
    if parts.len() != 2 {
        anyhow::bail!("Invalid format (expected KEY=value): {}", s);
    }

    let key = parts[0].trim();
    let value_part = parts[1];

    // Validate key name
    if key.is_empty() {
        anyhow::bail!("Invalid key name '' (must be alphanumeric with underscores)");
    }

    if !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        anyhow::bail!(
            "Invalid key name '{}' (must be alphanumeric with underscores)",
            key
        );
    }

    let (value, is_secret) = if let Some(stripped) = value_part.strip_prefix("secret:") {
        (stripped, true)
    } else {
        (value_part, false)
    };

    Ok(ParsedEnvVar {
        key: key.to_string(),
        value: value.to_string(),
        is_secret,
    })
}

/// Parse a multi-line env file (same format as `rise env import`)
///
/// Lines starting with # are comments, empty lines are ignored.
/// Format: KEY=value (plain text) or KEY=secret:value (secret)
pub fn parse_env_file(contents: &str) -> anyhow::Result<Vec<ParsedEnvVar>> {
    let mut vars = Vec::new();

    for (line_num, line) in iter_env_file_lines(contents) {
        match parse_env_string(line) {
            Ok(var) => vars.push(var),
            Err(e) => anyhow::bail!("Line {}: {}", line_num, e),
        }
    }

    Ok(vars)
}

/// Import environment variables from a file
///
/// File format:
/// - Lines starting with # are comments
/// - Empty lines are ignored
/// - Format: KEY=value (plain text) or KEY=secret:value (secret)
/// - Example:
///   ```
///   # Database configuration
///   DB_HOST=localhost
///   DB_PASSWORD=secret:my-secret-password
///   ```
pub async fn import_env(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    project: &str,
    file_path: &PathBuf,
    environment: Option<&str>,
) -> Result<()> {
    let contents = std::fs::read_to_string(file_path)
        .with_context(|| format!("Failed to read file: {}", file_path.display()))?;

    let mut success_count = 0;
    let mut error_count = 0;

    for (line_num, line) in iter_env_file_lines(&contents) {
        let parsed = match parse_env_string(line) {
            Ok(parsed) => parsed,
            Err(e) => {
                eprintln!("Warning: Line {}: {}", line_num, e);
                error_count += 1;
                continue;
            }
        };

        // Set the variable
        // Protected defaults to true for secrets, false for non-secrets
        let is_protected = parsed.is_secret;
        match set_env(
            http_client,
            backend_url,
            token,
            project,
            &parsed.key,
            &parsed.value,
            parsed.is_secret,
            is_protected,
            environment,
        )
        .await
        {
            Ok(_) => success_count += 1,
            Err(e) => {
                eprintln!(
                    "Warning: Failed to set variable '{}' from line {}: {}",
                    parsed.key, line_num, e
                );
                error_count += 1;
            }
        }
    }

    println!(
        "\n✓ Import complete: {} variables set, {} errors",
        success_count, error_count
    );

    if error_count > 0 {
        anyhow::bail!("Import completed with {} errors", error_count);
    }

    Ok(())
}

/// List environment variables for a deployment (read-only)
pub async fn list_deployment_env(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    project: &str,
    deployment_id: &str,
) -> Result<()> {
    let url = format!(
        "{}/api/v1/projects/{}/deployments/{}/env",
        backend_url, project, deployment_id
    );

    let response = http_client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
        .context("Failed to list deployment environment variables")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        anyhow::bail!(
            "Failed to list deployment environment variables (status {}): {}",
            status,
            error_text
        );
    }

    let env_vars_response: EnvVarsResponse = response
        .json()
        .await
        .context("Failed to parse environment variables response")?;

    if env_vars_response.env_vars.is_empty() {
        println!(
            "No environment variables configured for deployment '{}' in project '{}'",
            deployment_id, project
        );
        return Ok(());
    }

    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_header(vec![
            Cell::new("KEY").add_attribute(Attribute::Bold),
            Cell::new("VALUE").add_attribute(Attribute::Bold),
            Cell::new("TYPE").add_attribute(Attribute::Bold),
            Cell::new("SOURCE").add_attribute(Attribute::Bold),
        ]);

    for var in env_vars_response.env_vars {
        let var_type = if var.is_secret { "secret" } else { "plain" };
        let source = var.source.as_deref().unwrap_or("-");
        table.add_row(vec![
            Cell::new(&var.key),
            Cell::new(&var.value),
            Cell::new(var_type),
            Cell::new(source),
        ]);
    }

    println!("{}", table);
    println!("\nProject: {}", project);
    println!("Deployment: {}", deployment_id);
    println!("Note: Secret values are always masked for security");
    println!("Note: Deployment environment variables are read-only snapshots");

    Ok(())
}

/// Quote a value as a POSIX shell single-quoted string.
fn shell_quote(value: &str) -> anyhow::Result<String> {
    if value.contains('\0') {
        anyhow::bail!("Environment variable values cannot contain NUL bytes")
    }

    Ok(format!("'{}'", value.replace('\'', "'\\''")))
}

/// Check whether a key can be exported by a POSIX-compatible shell.
fn is_shell_identifier(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(
        chars.next(),
        Some(c) if c.is_ascii_alphabetic() || c == '_'
    ) && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Export environment variables as shell commands (`export KEY='value'`).
///
/// Output goes to stdout for clean piping (`eval "$(rise env export)"` or
/// `rise env export > .env.rise && source .env.rise`). Warnings about protected
/// secrets go to stderr.
pub async fn export_env(
    http_client: &Client,
    backend_url: &str,
    token: &str,
    project: &str,
    environment: Option<&str>,
) -> Result<()> {
    let (loadable_vars, protected_keys) = fetch_preview_env_vars(
        http_client,
        backend_url,
        token,
        project,
        "default",
        environment,
    )
    .await?;

    for (key, value) in &loadable_vars {
        if !is_shell_identifier(key) {
            anyhow::bail!(
                "Cannot export environment variable '{}': the key is not a valid shell identifier",
                key
            );
        }
        shell_quote(value)?;
    }

    if !protected_keys.is_empty() {
        eprintln!(
            "warning: {} protected secret{} excluded (cannot be exported):",
            protected_keys.len(),
            if protected_keys.len() == 1 { "" } else { "s" }
        );
        for key in &protected_keys {
            eprintln!("  - {}", key);
        }
    }

    for (key, value) in &loadable_vars {
        println!("export {}={}", key, shell_quote(value)?);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        is_shell_identifier, parse_env_file, parse_env_string, read_plain_env_value,
        resolve_env_value, shell_quote,
    };

    #[test]
    fn supplied_env_value_does_not_prompt() {
        assert_eq!(
            resolve_env_value(Some("  value  "), false).unwrap(),
            "  value  "
        );
    }

    #[test]
    fn plain_env_prompt_removes_only_the_line_ending() {
        let mut input = &b"  value  \r\n"[..];
        let mut output = Vec::new();

        assert_eq!(
            read_plain_env_value(&mut input, &mut output).unwrap(),
            "  value  "
        );
        assert_eq!(output, b"Value: ");
    }

    #[test]
    fn shell_quote_preserves_shell_significant_values() {
        assert_eq!(shell_quote("").unwrap(), "''");
        assert_eq!(shell_quote("hello world").unwrap(), "'hello world'");
        assert_eq!(shell_quote("it's").unwrap(), "'it'\\''s'");
        assert_eq!(
            shell_quote("line one\nline two").unwrap(),
            "'line one\nline two'"
        );
        assert_eq!(
            shell_quote("$(touch /tmp/pwned); *.txt").unwrap(),
            "'$(touch /tmp/pwned); *.txt'"
        );
    }

    #[test]
    fn shell_quote_rejects_nul_bytes() {
        assert!(shell_quote("before\0after").is_err());
    }

    #[test]
    fn shell_identifier_requires_portable_variable_name() {
        assert!(is_shell_identifier("FOO_2"));
        assert!(is_shell_identifier("_private"));
        assert!(!is_shell_identifier("2FOO"));
        assert!(!is_shell_identifier("foo.bar"));
        assert!(!is_shell_identifier("foo-bar"));
    }

    #[test]
    fn parse_env_string_rejects_empty_keys() {
        let err = parse_env_string("=value").unwrap_err();
        assert_eq!(
            err.to_string(),
            "Invalid key name '' (must be alphanumeric with underscores)"
        );
    }

    #[test]
    fn parse_env_file_reports_original_line_numbers() {
        let err = parse_env_file("\n# comment\n=value").unwrap_err();
        assert_eq!(
            err.to_string(),
            "Line 3: Invalid key name '' (must be alphanumeric with underscores)"
        );
    }

    #[test]
    fn parse_env_string_supports_secret_values() {
        let parsed = parse_env_string("API_KEY=secret:value").unwrap();

        assert_eq!(parsed.key, "API_KEY");
        assert_eq!(parsed.value, "value");
        assert!(parsed.is_secret);
    }
}
