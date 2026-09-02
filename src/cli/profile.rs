//! `rise profile` — inspect and manage the login profiles selected via the
//! global `--profile` flag, `RISE_PROFILE` environment variable, or persisted
//! default selection.
//!
//! A profile is "registered" simply by having a saved config file: `rise
//! login --profile <name>` creates one on first use, so there is nothing to
//! create explicitly ahead of time.

use crate::config::Config;
use anyhow::Result;
use comfy_table::{modifiers::UTF8_ROUND_CORNERS, presets::UTF8_FULL, Attribute, Cell, Table};

/// List all registered profiles, marking which one is currently active.
pub fn list_profiles() -> Result<()> {
    let active = Config::active_profile_label()?;

    let mut names = vec!["default".to_string()];
    names.extend(Config::list_profiles()?);

    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_header(vec![
            Cell::new("").add_attribute(Attribute::Bold),
            Cell::new("NAME").add_attribute(Attribute::Bold),
            Cell::new("BACKEND URL").add_attribute(Attribute::Bold),
            Cell::new("LOGGED IN").add_attribute(Attribute::Bold),
            Cell::new("CONFIG FILE").add_attribute(Attribute::Bold),
        ]);

    for name in &names {
        let key = if name == "default" {
            None
        } else {
            Some(name.as_str())
        };
        let cfg = Config::load_named(key)?;
        let path = Config::path_for(key)?;

        table.add_row(vec![
            Cell::new(if *name == active { "*" } else { "" }),
            Cell::new(name),
            Cell::new(cfg.backend_url.as_deref().unwrap_or("-")),
            Cell::new(if cfg.stored_token().is_some() {
                "yes"
            } else {
                "no"
            }),
            Cell::new(path.display()),
        ]);
    }

    println!("{}", table);
    println!("\nActive profile: {}", active);

    Ok(())
}

/// Select the profile used when no per-command or environment override exists.
pub fn use_profile(name: &str) -> Result<()> {
    Config::set_default_profile(name)?;
    println!("✓ Default profile set to '{}'", name);
    Ok(())
}

/// Remove a profile's saved config file.
pub fn remove_profile(name: &str) -> Result<()> {
    Config::remove_profile(name)?;
    println!("✓ Removed profile '{}'", name);
    Ok(())
}
