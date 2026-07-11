use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

/// Repository that owns the bundled skills.
const REPO_OWNER: &str = "rise-deploy";
const REPO_NAME: &str = "rise";
/// Directory inside the repo where skills live.
const SKILLS_DIR: &str = "skills";
/// The skill installed by default when no name is given.
const DEFAULT_SKILL: &str = "rise-app-builder";
const DEFAULT_GIT_REF: &str = "develop";

#[derive(Deserialize)]
struct TreeResponse {
    #[serde(default)]
    tree: Vec<TreeEntry>,
    #[serde(default)]
    truncated: bool,
}

#[derive(Deserialize)]
struct CommitResponse {
    sha: String,
}

#[derive(Deserialize)]
struct TreeEntry {
    path: String,
    #[serde(rename = "type")]
    type_: String,
}

/// Default install location for Crush / Claude Code skills.
fn default_target() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Failed to get home directory")?;
    Ok(home.join(".claude").join("skills"))
}

fn validate_skill_name(name: &str) -> Result<()> {
    let mut components = Path::new(name).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(component)), None) if !component.is_empty() => Ok(()),
        _ => bail!(
            "Invalid skill name '{}'. Use a single directory name.",
            name
        ),
    }
}

fn validate_install_destination(path: &Path, name: &str) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && path.join("SKILL.md").is_file() => Ok(()),
        Ok(_) => bail!(
            "Refusing to replace '{}': {} is not an installed skill directory.",
            name,
            path.display(),
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("Failed to inspect {}", path.display())),
    }
}

fn path_exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("Failed to inspect {}", path.display())),
    }
}

fn backup_path(target: &Path, name: &str) -> PathBuf {
    target.join(format!(".{name}.previous"))
}

fn is_internal_entry(name: &str) -> bool {
    name.starts_with('.') && (name.ends_with(".previous") || name.contains(".installing-"))
}

fn recover_interrupted_install(dest: &Path, backup: &Path, name: &str) -> Result<()> {
    let dest_exists = path_exists(dest)?;
    let backup_exists = path_exists(backup)?;

    match (dest_exists, backup_exists) {
        (false, true) => {
            validate_install_destination(backup, name)?;
            std::fs::rename(backup, dest).with_context(|| {
                format!(
                    "Failed to restore interrupted skill update from {} to {}",
                    backup.display(),
                    dest.display(),
                )
            })?;
            println!("Restored interrupted skill update for '{}'.", name);
        }
        (true, true) => {
            validate_install_destination(dest, name)?;
            validate_install_destination(backup, name)?;
            std::fs::remove_dir_all(backup).with_context(|| {
                format!(
                    "Failed to clean previous skill version {} before updating",
                    backup.display(),
                )
            })?;
        }
        _ => {}
    }

    Ok(())
}

fn github_commit_url(git_ref: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(&format!(
        "https://api.github.com/repos/{}/{}/commits/",
        REPO_OWNER, REPO_NAME,
    ))?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("Failed to construct GitHub commit URL"))?
        .pop_if_empty()
        .push(git_ref);
    Ok(url)
}

fn github_tree_url(commit_sha: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(&format!(
        "https://api.github.com/repos/{}/{}/git/trees/",
        REPO_OWNER, REPO_NAME,
    ))?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("Failed to construct GitHub tree URL"))?
        .pop_if_empty()
        .push(commit_sha);
    url.query_pairs_mut().append_pair("recursive", "1");
    Ok(url)
}

fn github_raw_url(commit_sha: &str, path: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(&format!(
        "https://raw.githubusercontent.com/{}/{}/",
        REPO_OWNER, REPO_NAME,
    ))?;
    let mut segments = url
        .path_segments_mut()
        .map_err(|_| anyhow::anyhow!("Failed to construct GitHub raw URL"))?;
    segments.pop_if_empty();
    segments.push(commit_sha);
    for component in path.split('/') {
        segments.push(component);
    }
    drop(segments);
    Ok(url)
}

fn staging_path(target: &Path, name: &str, suffix: &str) -> Result<PathBuf> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("System clock is before the Unix epoch")?
        .as_nanos();
    Ok(target.join(format!(".{name}.{suffix}-{}-{nonce}", std::process::id())))
}

/// Install (or update) a Rise-bundled skill by fetching it from GitHub.
pub async fn install_command(
    name: Option<String>,
    git_ref: Option<String>,
    target: Option<PathBuf>,
) -> Result<()> {
    let name = name.unwrap_or_else(|| DEFAULT_SKILL.to_string());
    validate_skill_name(&name)?;
    let git_ref = git_ref.unwrap_or_else(|| DEFAULT_GIT_REF.to_string());
    let target = match target {
        Some(t) => t,
        None => default_target()?,
    };
    std::fs::create_dir_all(&target)
        .with_context(|| format!("Failed to create skills directory {}", target.display()))?;
    let dest_root = target.join(&name);
    let backup_root = backup_path(&target, &name);
    recover_interrupted_install(&dest_root, &backup_root, &name)?;
    validate_install_destination(&dest_root, &name)?;

    let http = reqwest::Client::builder()
        .user_agent(concat!("rise/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()?;

    // 1. Resolve the requested ref to an immutable commit before listing or
    //    downloading files, so a moving branch cannot mix revisions.
    let commit_url = github_commit_url(&git_ref)?;
    let resp = http
        .get(commit_url.as_str())
        .send()
        .await
        .context("Failed to reach GitHub API. Check your network connection and try again.")?;
    if resp.status() == reqwest::StatusCode::FORBIDDEN
        || resp.status() == reqwest::StatusCode::UNPROCESSABLE_ENTITY
    {
        bail!(
            "GitHub refused the request (status {}). The ref '{}' may not exist, \
             or the unauthenticated API rate limit was hit (60/hour/IP). \
             Wait and retry, or specify a valid --git-ref.",
            resp.status(),
            git_ref,
        );
    }
    if !resp.status().is_success() {
        bail!(
            "GitHub API returned status {} resolving ref '{}'",
            resp.status(),
            git_ref,
        );
    }
    let commit: CommitResponse = resp
        .json()
        .await
        .context("Failed to parse GitHub commit response")?;

    let tree_url = github_tree_url(&commit.sha)?;
    let resp = http
        .get(tree_url.as_str())
        .send()
        .await
        .context("Failed to fetch the GitHub repository tree")?;
    if !resp.status().is_success() {
        bail!(
            "GitHub API returned status {} fetching tree for ref '{}'",
            resp.status(),
            git_ref,
        );
    }
    let tree: TreeResponse = resp
        .json()
        .await
        .context("Failed to parse GitHub tree response")?;
    if tree.truncated {
        bail!(
            "GitHub returned a truncated repository tree for ref '{}'; refusing a partial install.",
            git_ref,
        );
    }

    // 2. Collect blob paths under skills/<name>/.
    let prefix = format!("{}/{}/", SKILLS_DIR, name);
    let blobs: Vec<&TreeEntry> = tree
        .tree
        .iter()
        .filter(|e| e.type_ == "blob" && e.path.starts_with(&prefix))
        .collect();

    if blobs.is_empty() {
        bail!(
            "Skill '{}' not found under '{}/' in repo {}/{} at ref '{}'. \
             Check the name, or open a PR to add it.",
            name,
            SKILLS_DIR,
            REPO_OWNER,
            REPO_NAME,
            git_ref,
        );
    }

    println!(
        "Installing skill '{}' (ref {}) from {}/{} into {}",
        name,
        git_ref,
        REPO_OWNER,
        REPO_NAME,
        dest_root.display(),
    );

    // 3. Fetch every file before touching the installed version.
    let mut files = Vec::with_capacity(blobs.len());
    for blob in &blobs {
        let rel = &blob.path[prefix.len()..];
        let raw_url = github_raw_url(&commit.sha, &blob.path)?;
        let body = http
            .get(raw_url.as_str())
            .send()
            .await
            .with_context(|| format!("Failed to fetch {}", raw_url))?
            .error_for_status()
            .with_context(|| format!("GitHub returned an error for {}", raw_url))?
            .bytes()
            .await
            .with_context(|| format!("Failed to read {}", raw_url))?;
        files.push((rel, body));
    }

    // 4. Build the new version beside the live directory, then swap it in.
    let staging_root = staging_path(&target, &name, "installing")?;
    std::fs::create_dir(&staging_root).with_context(|| {
        format!(
            "Failed to create staging directory {}",
            staging_root.display()
        )
    })?;

    for (rel, body) in &files {
        let dest = staging_root.join(rel);
        if let Some(parent) = dest.parent() {
            if let Err(error) = std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {:?}", parent))
            {
                let _ = std::fs::remove_dir_all(&staging_root);
                return Err(error);
            }
        }
        if let Err(error) =
            std::fs::write(&dest, body).with_context(|| format!("Failed to write {:?}", dest))
        {
            let _ = std::fs::remove_dir_all(&staging_root);
            return Err(error);
        }
        println!("  wrote {}", rel);
    }

    if dest_root.exists() {
        if let Err(error) = std::fs::rename(&dest_root, &backup_root).with_context(|| {
            format!(
                "Failed to move existing skill {} out of the way",
                dest_root.display()
            )
        }) {
            let _ = std::fs::remove_dir_all(&staging_root);
            return Err(error);
        }
    }
    if let Err(error) = std::fs::rename(&staging_root, &dest_root) {
        if backup_root.exists() {
            if let Err(restore_error) = std::fs::rename(&backup_root, &dest_root) {
                let _ = std::fs::remove_dir_all(&staging_root);
                bail!(
                    "Failed to activate installed skill {}: {}. The previous version remains at {} because restoring it also failed: {}",
                    dest_root.display(),
                    error,
                    backup_root.display(),
                    restore_error,
                );
            }
        }
        let cleanup_error = std::fs::remove_dir_all(&staging_root).err();
        let activation_error = Err(error)
            .with_context(|| format!("Failed to activate installed skill {}", dest_root.display()));
        return match cleanup_error {
            Some(cleanup_error) => activation_error.with_context(|| {
                format!(
                    "Also failed to remove staging directory {}: {}",
                    staging_root.display(),
                    cleanup_error,
                )
            }),
            None => activation_error,
        };
    }
    if backup_root.exists() {
        if let Err(error) = std::fs::remove_dir_all(&backup_root) {
            eprintln!(
                "Warning: installed skill, but failed to remove previous version {}: {}",
                backup_root.display(),
                error,
            );
        }
    }

    println!(
        "\nInstalled skill '{}'. {} file(s) written.\n\
         Restart your AI tool (Crush / Claude Code) so it picks up the new skill. \
         To target a different directory, pass --target <dir>.",
        name,
        files.len(),
    );
    Ok(())
}

/// List skills installed in the target directory.
pub fn list_command(target: Option<PathBuf>) -> Result<()> {
    let target = match target {
        Some(t) => t,
        None => default_target()?,
    };
    if !target.exists() {
        println!("No skills installed in {}.", target.display());
        return Ok(());
    }
    let mut found: Vec<(String, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(&target)
        .with_context(|| format!("Failed to read {}", target.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_internal_entry)
        {
            continue;
        }
        if path.is_dir() && path.join("SKILL.md").exists() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                found.push((name.to_string(), path));
            }
        }
    }
    found.sort();
    if found.is_empty() {
        println!("No skills installed in {}.", target.display());
        return Ok(());
    }
    println!("{:<24} PATH", "SKILL");
    for (name, path) in found {
        println!("{:<24} {}", name, path.display());
    }
    Ok(())
}

/// Remove an installed skill.
pub fn uninstall_command(name: String, target: Option<PathBuf>) -> Result<()> {
    validate_skill_name(&name)?;
    let target = match target {
        Some(t) => t,
        None => default_target()?,
    };
    let dir = target.join(&name);
    if !dir.exists() {
        bail!("Skill '{}' is not installed in {}.", name, target.display());
    }
    let metadata = std::fs::symlink_metadata(&dir)
        .with_context(|| format!("Failed to inspect {}", dir.display()))?;
    if !metadata.is_dir() || !dir.join("SKILL.md").is_file() {
        bail!(
            "Refusing to remove '{}': {} is not an installed skill directory.",
            name,
            dir.display(),
        );
    }
    remove_dir_all(&dir).with_context(|| format!("Failed to remove {}", dir.display()))?;
    println!("Uninstalled skill '{}' from {}.", name, dir.display());
    Ok(())
}

/// std::fs::remove_dir_all is available on all targets.
fn remove_dir_all(path: &Path) -> std::io::Result<()> {
    std::fs::remove_dir_all(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_single_component_skill_names() {
        validate_skill_name("rise-app-builder").unwrap();
        validate_skill_name("skill_name").unwrap();
    }

    #[test]
    fn rejects_skill_names_that_can_escape_the_target() {
        for name in ["", ".", "..", "../outside", "nested/skill", "/tmp/outside"] {
            assert!(validate_skill_name(name).is_err(), "accepted {name:?}");
        }
    }

    #[test]
    fn encodes_git_refs_as_single_url_segments() {
        let url = github_commit_url("feature/skill install").unwrap();
        assert_eq!(
            url.as_str(),
            "https://api.github.com/repos/rise-deploy/rise/commits/feature%2Fskill%20install"
        );
    }

    #[test]
    fn install_refuses_non_skill_destinations() {
        let target = tempfile::tempdir().unwrap();
        let destination = target.path().join("not-a-skill");
        std::fs::create_dir(&destination).unwrap();

        let error = validate_install_destination(&destination, "not-a-skill").unwrap_err();

        assert!(error
            .to_string()
            .contains("not an installed skill directory"));
        assert!(destination.exists());
    }

    #[test]
    fn install_accepts_existing_skill_destinations() {
        let target = tempfile::tempdir().unwrap();
        let destination = target.path().join("rise-app-builder");
        std::fs::create_dir(&destination).unwrap();
        std::fs::write(destination.join("SKILL.md"), "# Skill").unwrap();

        validate_install_destination(&destination, "rise-app-builder").unwrap();
    }

    #[test]
    fn recovers_previous_version_after_interrupted_install() {
        let target = tempfile::tempdir().unwrap();
        let destination = target.path().join("rise-app-builder");
        let backup = backup_path(target.path(), "rise-app-builder");
        std::fs::create_dir(&backup).unwrap();
        std::fs::write(backup.join("SKILL.md"), "# Previous").unwrap();

        recover_interrupted_install(&destination, &backup, "rise-app-builder").unwrap();

        assert!(destination.join("SKILL.md").is_file());
        assert!(!backup.exists());
    }

    #[test]
    fn cleans_stale_backup_after_successful_activation() {
        let target = tempfile::tempdir().unwrap();
        let destination = target.path().join("rise-app-builder");
        let backup = backup_path(target.path(), "rise-app-builder");
        for path in [&destination, &backup] {
            std::fs::create_dir(path).unwrap();
            std::fs::write(path.join("SKILL.md"), "# Skill").unwrap();
        }

        recover_interrupted_install(&destination, &backup, "rise-app-builder").unwrap();

        assert!(destination.join("SKILL.md").is_file());
        assert!(!backup.exists());
    }

    #[test]
    fn recognizes_only_internal_transaction_entries() {
        assert!(is_internal_entry(".rise-app-builder.previous"));
        assert!(is_internal_entry(".rise-app-builder.installing-123-456"));
        assert!(!is_internal_entry("rise-app-builder"));
        assert!(!is_internal_entry(".custom-skill"));
    }

    #[test]
    fn uninstall_refuses_non_skill_directories() {
        let target = tempfile::tempdir().unwrap();
        std::fs::create_dir(target.path().join("not-a-skill")).unwrap();

        let error = uninstall_command("not-a-skill".to_string(), Some(target.path().to_path_buf()))
            .unwrap_err();

        assert!(error
            .to_string()
            .contains("not an installed skill directory"));
        assert!(target.path().join("not-a-skill").exists());
    }

    #[test]
    fn uninstall_removes_skill_directories() {
        let target = tempfile::tempdir().unwrap();
        let skill = target.path().join("rise-app-builder");
        std::fs::create_dir(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "# Skill").unwrap();

        uninstall_command(
            "rise-app-builder".to_string(),
            Some(target.path().to_path_buf()),
        )
        .unwrap();

        assert!(!skill.exists());
    }
}
