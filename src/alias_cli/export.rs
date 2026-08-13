//! `zeroclaw agents export` — write an agent bundle to disk.
//!
//! The closure computation, credential scrubbing, and risk analysis all live
//! in [`zeroclaw_config::agent_bundle`]. This module is the I/O half: it
//! materializes a plan into a directory and reports to the operator what the
//! bundle carries, what it left behind, and what a receiving install would be
//! asked to grant.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use zeroclaw_config::agent_bundle::{
    self, CONFIG_FILE, ExportOptions, ExportPlan, MANIFEST_FILE, Provenance, WORKSPACE_DIR,
};
use zeroclaw_config::schema::Config;

use super::{mt, mta};

/// Outcome of copying the agent's workspace into the bundle.
#[derive(Debug, Default, PartialEq, Eq)]
struct WorkspaceCopy {
    files: usize,
    bytes: u64,
    /// Symlinks encountered and skipped. They are not followed: a link's
    /// target may sit outside the workspace, and it would resolve to
    /// something different on the receiving host regardless.
    symlinks_skipped: usize,
}

pub async fn run(
    config: &Config,
    alias: &str,
    out: &Path,
    include_memory: bool,
    force: bool,
) -> Result<()> {
    let plan = agent_bundle::plan_export(config, alias, ExportOptions { include_memory })
        .map_err(anyhow::Error::new)?;

    prepare_destination(out, force).await?;

    let config_toml = agent_bundle::render_config_toml(&plan).map_err(anyhow::Error::new)?;
    let manifest_toml = agent_bundle::render_manifest_toml(
        &plan,
        &Provenance {
            zeroclaw_version: env!("CARGO_PKG_VERSION").to_string(),
            exported_at: chrono::Utc::now().to_rfc3339(),
        },
    )
    .map_err(anyhow::Error::new)?;

    write_file(&out.join(CONFIG_FILE), &config_toml).await?;
    write_file(&out.join(MANIFEST_FILE), &manifest_toml).await?;

    let copied = copy_workspace(&plan, &out.join(WORKSPACE_DIR))?;

    report(&plan, out, &copied);
    Ok(())
}

/// Create the destination, refusing to write into a directory that already
/// holds files unless the operator passed `--force`. An export that silently
/// merges into a previous bundle would produce a bundle describing two agents.
async fn prepare_destination(out: &Path, force: bool) -> Result<()> {
    if out.exists() {
        if !out.is_dir() {
            bail!(
                "{}",
                mta(
                    "cli-agent-export-dest-not-a-dir",
                    &[("path", out.display().to_string().as_str())],
                    "destination {$path} exists and is not a directory"
                )
            );
        }
        let mut entries = tokio::fs::read_dir(out)
            .await
            .with_context(|| format!("failed to read destination {}", out.display()))?;
        let occupied = entries
            .next_entry()
            .await
            .with_context(|| format!("failed to read destination {}", out.display()))?
            .is_some();
        if occupied && !force {
            bail!(
                "{}",
                mta(
                    "cli-agent-export-dest-not-empty",
                    &[("path", out.display().to_string().as_str())],
                    "destination {$path} is not empty — pass --force to write into it anyway"
                )
            );
        }
        return Ok(());
    }
    tokio::fs::create_dir_all(out)
        .await
        .with_context(|| format!("failed to create destination {}", out.display()))
}

async fn write_file(path: &Path, contents: &str) -> Result<()> {
    tokio::fs::write(path, contents)
        .await
        .with_context(|| format!("failed to write {}", path.display()))
}

/// Copy the agent's workspace into the bundle, honoring the plan's memory
/// exclusion. A missing source workspace is not an error: an agent that has
/// never run has nothing on disk yet.
fn copy_workspace(plan: &ExportPlan, dest: &Path) -> Result<WorkspaceCopy> {
    let mut copied = WorkspaceCopy::default();
    if !plan.workspace_source.is_dir() {
        return Ok(copied);
    }
    std::fs::create_dir_all(dest)
        .with_context(|| format!("failed to create {}", dest.display()))?;
    copy_tree(
        &plan.workspace_source,
        dest,
        &PathBuf::new(),
        plan.include_memory,
        &mut copied,
    )?;
    Ok(copied)
}

fn copy_tree(
    source: &Path,
    dest: &Path,
    relative: &Path,
    include_memory: bool,
    copied: &mut WorkspaceCopy,
) -> Result<()> {
    let entries = std::fs::read_dir(source)
        .with_context(|| format!("failed to read {}", source.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("failed to read {}", source.display()))?;
        let name = entry.file_name();
        let child_relative = relative.join(&name);
        if !agent_bundle::workspace_entry_included(&child_relative, include_memory) {
            continue;
        }

        let source_path = entry.path();
        let metadata = std::fs::symlink_metadata(&source_path)
            .with_context(|| format!("failed to stat {}", source_path.display()))?;
        if metadata.file_type().is_symlink() {
            copied.symlinks_skipped += 1;
            continue;
        }

        let dest_path = dest.join(&name);
        if metadata.is_dir() {
            std::fs::create_dir_all(&dest_path)
                .with_context(|| format!("failed to create {}", dest_path.display()))?;
            copy_tree(
                &source_path,
                &dest_path,
                &child_relative,
                include_memory,
                copied,
            )?;
        } else if metadata.is_file() {
            std::fs::copy(&source_path, &dest_path).with_context(|| {
                format!(
                    "failed to copy {} to {}",
                    source_path.display(),
                    dest_path.display()
                )
            })?;
            copied.files += 1;
            copied.bytes += metadata.len();
        }
    }
    Ok(())
}

fn report(plan: &ExportPlan, out: &Path, copied: &WorkspaceCopy) {
    let files = copied.files.to_string();
    let kib = (copied.bytes / 1024).to_string();
    println!(
        "{}",
        mta(
            "cli-agent-export-written",
            &[
                ("alias", plan.root_alias.as_str()),
                ("path", out.display().to_string().as_str()),
                ("files", files.as_str()),
                ("kib", kib.as_str()),
            ],
            "exported agent `{$alias}` to {$path} ({$files} workspace file(s), {$kib} KiB)"
        )
    );

    if copied.symlinks_skipped > 0 {
        let count = copied.symlinks_skipped.to_string();
        println!(
            "{}",
            mta(
                "cli-agent-export-symlinks-skipped",
                &[("count", count.as_str())],
                "  {$count} symlink(s) skipped — links are not followed into a bundle"
            )
        );
    }

    if !plan.risk_flags.is_empty() {
        let count = plan.risk_flags.len().to_string();
        println!(
            "\n{}",
            mta(
                "cli-agent-export-risk-header",
                &[("count", count.as_str())],
                "⚠️  {$count} capability grant(s) an importing operator must accept:"
            )
        );
        for flag in &plan.risk_flags {
            println!(
                "{}",
                mta(
                    "cli-agent-export-risk-entry",
                    &[
                        ("kind", flag.kind.as_wire()),
                        ("path", flag.path.as_str()),
                        ("detail", flag.detail.as_str()),
                    ],
                    "  [{$kind}] {$path} — {$detail}"
                )
            );
        }
    }

    if !plan.required_secrets.is_empty() {
        let count = plan.required_secrets.len().to_string();
        println!(
            "\n{}",
            mta(
                "cli-agent-export-secrets-header",
                &[("count", count.as_str())],
                "🔑 {$count} credential(s) were scrubbed and must be supplied on import:"
            )
        );
        for path in &plan.required_secrets {
            println!(
                "{}",
                mta(
                    "cli-agent-export-secrets-entry",
                    &[("path", path.as_str())],
                    "  {$path}"
                )
            );
        }
    }

    if !plan.dropped.is_empty() {
        let count = plan.dropped.len().to_string();
        println!(
            "\n{}",
            mta(
                "cli-agent-export-dropped-header",
                &[("count", count.as_str())],
                "ℹ️  {$count} item(s) could not travel and were left behind:"
            )
        );
        for entry in &plan.dropped {
            println!(
                "{}",
                mta(
                    "cli-agent-export-dropped-entry",
                    &[
                        ("path", entry.path.as_str()),
                        ("reason", entry.reason.as_wire()),
                        ("detail", entry.detail.as_str()),
                    ],
                    "  {$path} ({$reason}) — {$detail}"
                )
            );
        }
    }

    println!(
        "\n{}",
        mt(
            "cli-agent-export-review-hint",
            "Review config.toml and zeroclaw-agent.toml before sharing the bundle."
        )
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    fn plan_for(workspace: &Path, include_memory: bool) -> ExportPlan {
        ExportPlan {
            root_alias: "researcher".to_string(),
            config: toml::Table::new(),
            required_secrets: Vec::new(),
            dropped: Vec::new(),
            risk_flags: Vec::new(),
            workspace_source: workspace.to_path_buf(),
            include_memory,
        }
    }

    #[test]
    fn copy_skips_the_memory_store_by_default() {
        let source = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        write(&source.path().join("IDENTITY.md"), "identity");
        write(&source.path().join("notes/plan.md"), "plan");
        write(&source.path().join("memory/brain.db"), "sqlite");

        let plan = plan_for(source.path(), false);
        let copied = copy_workspace(&plan, dest.path()).unwrap();

        assert_eq!(copied.files, 2);
        assert!(dest.path().join("IDENTITY.md").exists());
        assert!(dest.path().join("notes/plan.md").exists());
        assert!(!dest.path().join("memory").exists());
    }

    #[test]
    fn copy_carries_the_memory_store_when_requested() {
        let source = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        write(&source.path().join("IDENTITY.md"), "identity");
        write(&source.path().join("memory/brain.db"), "sqlite");

        let plan = plan_for(source.path(), true);
        let copied = copy_workspace(&plan, dest.path()).unwrap();

        assert_eq!(copied.files, 2);
        assert!(dest.path().join("memory/brain.db").exists());
    }

    #[cfg(unix)]
    #[test]
    fn copy_skips_symlinks_instead_of_following_them_out_of_the_workspace() {
        let outside = tempfile::tempdir().unwrap();
        write(&outside.path().join("secret.txt"), "host secret");

        let source = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        write(&source.path().join("IDENTITY.md"), "identity");
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            source.path().join("escape.txt"),
        )
        .unwrap();

        let plan = plan_for(source.path(), false);
        let copied = copy_workspace(&plan, dest.path()).unwrap();

        assert_eq!(copied.files, 1);
        assert_eq!(copied.symlinks_skipped, 1);
        assert!(!dest.path().join("escape.txt").exists());
    }

    #[test]
    fn missing_workspace_is_not_an_error() {
        let dest = tempfile::tempdir().unwrap();
        let plan = plan_for(Path::new("/nonexistent/zeroclaw/workspace"), false);
        let copied = copy_workspace(&plan, dest.path()).unwrap();
        assert_eq!(copied, WorkspaceCopy::default());
    }

    #[tokio::test]
    async fn non_empty_destination_is_refused_without_force() {
        let dest = tempfile::tempdir().unwrap();
        write(&dest.path().join("config.toml"), "# existing");

        let err = prepare_destination(dest.path(), false).await.unwrap_err();
        assert!(err.to_string().contains("--force"), "{err}");

        prepare_destination(dest.path(), true).await.unwrap();
    }

    #[tokio::test]
    async fn empty_and_absent_destinations_are_accepted() {
        let existing = tempfile::tempdir().unwrap();
        prepare_destination(existing.path(), false).await.unwrap();

        let parent = tempfile::tempdir().unwrap();
        let fresh = parent.path().join("bundle");
        prepare_destination(&fresh, false).await.unwrap();
        assert!(fresh.is_dir());
    }
}
