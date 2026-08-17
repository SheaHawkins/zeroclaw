//! `zeroclaw agents export` — write an agent bundle to disk.
//!
//! The closure computation, credential scrubbing, and risk analysis all live
//! in [`zeroclaw_config::agent_bundle`]. This module is the I/O half: it
//! materializes a plan into a directory and reports to the operator what the
//! bundle carries, what it left behind, and what a receiving install would be
//! asked to grant.
//!
//! A bundle is published, not merged: it is built in a staging directory beside
//! the destination and swapped into place in one move. An export therefore
//! either replaces the destination with a complete bundle or leaves it exactly
//! as it found it — a half-written bundle and a bundle carrying leftovers from
//! an earlier export are both states a receiving operator would have no way to
//! detect from the manifest.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use zeroclaw_config::agent_bundle::{
    self, CONFIG_FILE, ExportOptions, ExportPlan, MANIFEST_FILE, Provenance, WORKSPACE_DIR,
};
use zeroclaw_config::schema::Config;

use super::{mt, mta};

/// Staging directory prefix. Staging lives beside the destination so the
/// publishing rename stays within one filesystem.
const STAGING_PREFIX: &str = ".zeroclaw-export-";

/// Prefix for the previous bundle while the new one is being moved into place.
const RETIRED_PREFIX: &str = ".zeroclaw-export-old-";

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

    let copied = write_bundle(&plan, out, force).await?;

    report(&plan, out, &copied);
    Ok(())
}

/// Materialize `plan` at `out`.
///
/// Everything that can refuse the export runs first, against nothing but
/// metadata; only then is a staging directory created, filled, and swapped in.
/// The destination is never partially written and never keeps an entry the new
/// manifest does not describe.
async fn write_bundle(plan: &ExportPlan, out: &Path, force: bool) -> Result<WorkspaceCopy> {
    let dest = resolve_path(out)?;
    reject_workspace_overlap(&dest, plan, out)?;
    check_destination(&dest, out, force).await?;

    let config_toml = agent_bundle::render_config_toml(plan).map_err(anyhow::Error::new)?;
    let manifest_toml = agent_bundle::render_manifest_toml(
        plan,
        &Provenance {
            zeroclaw_version: env!("CARGO_PKG_VERSION").to_string(),
            exported_at: chrono::Utc::now().to_rfc3339(),
        },
    )
    .map_err(anyhow::Error::new)?;

    let Some(parent) = dest.parent() else {
        bail!(
            "{}",
            mta(
                "cli-agent-export-dest-no-parent",
                &[("path", out.display().to_string().as_str())],
                "destination {$path} has no parent directory to stage the bundle beside"
            )
        );
    };
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("failed to create destination parent {}", parent.display()))?;

    // Dropping the staging directory removes it, so every `?` below cleans up
    // after itself and leaves the destination untouched.
    let staging = tempfile::Builder::new()
        .prefix(STAGING_PREFIX)
        .tempdir_in(parent)
        .with_context(|| {
            format!(
                "failed to create a staging directory for the bundle in {}",
                parent.display()
            )
        })?;

    write_file(&staging.path().join(CONFIG_FILE), &config_toml).await?;
    write_file(&staging.path().join(MANIFEST_FILE), &manifest_toml).await?;
    let copied = copy_workspace(plan, &staging.path().join(WORKSPACE_DIR))?;

    publish(staging, &dest, parent)?;
    Ok(copied)
}

/// Resolve `path` to an absolute, symlink-free form.
///
/// The destination need not exist yet, so the nearest existing ancestor is
/// canonicalized and the components below it are re-appended. Resolving both
/// sides this way is what makes [`reject_workspace_overlap`] trustworthy: a
/// symlinked ancestor (`/tmp` → `/private/tmp` on macOS, an operator's
/// symlinked data dir anywhere) would otherwise hide an overlap.
fn resolve_path(path: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(path)
        .with_context(|| format!("failed to resolve {}", path.display()))?;
    let mut below: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = absolute.as_path();
    loop {
        if let Ok(canonical) = cursor.canonicalize() {
            let mut resolved = canonical;
            resolved.extend(below.iter().rev());
            return Ok(resolved);
        }
        match (cursor.file_name(), cursor.parent()) {
            (Some(name), Some(parent)) => {
                below.push(name.to_os_string());
                cursor = parent;
            }
            // No ancestor exists at all (an absolute path under a missing
            // root); nothing can be canonicalized, so compare it as written.
            _ => return Ok(absolute),
        }
    }
}

/// Refuse an export whose destination and source workspace overlap in either
/// direction.
///
/// A destination containing the workspace would have publishing replace the
/// very tree the bundle is reading, destroying the agent's workspace. A
/// destination inside the workspace would have the copy walk into its own
/// output. Both are refused before anything is created.
fn reject_workspace_overlap(dest: &Path, plan: &ExportPlan, out: &Path) -> Result<()> {
    let workspace = resolve_path(&plan.workspace_source)?;
    let dest_display = out.display().to_string();
    let workspace_display = plan.workspace_source.display().to_string();
    // Containment is checked first so that a destination equal to the
    // workspace reports the destructive shape rather than the recursive one.
    if workspace.starts_with(dest) {
        bail!(
            "{}",
            mta(
                "cli-agent-export-dest-contains-workspace",
                &[
                    ("path", dest_display.as_str()),
                    ("workspace", workspace_display.as_str())
                ],
                "destination {$path} contains the agent workspace {$workspace} — exporting there would replace the workspace itself"
            )
        );
    }
    if dest.starts_with(&workspace) {
        bail!(
            "{}",
            mta(
                "cli-agent-export-dest-inside-workspace",
                &[
                    ("path", dest_display.as_str()),
                    ("workspace", workspace_display.as_str())
                ],
                "destination {$path} is inside the agent workspace {$workspace} — choose a path outside it"
            )
        );
    }
    Ok(())
}

/// Check that the destination can be published to, without touching it. A
/// non-directory is refused outright; a directory that already holds files
/// needs `--force`, which replaces its contents rather than merging into them.
async fn check_destination(dest: &Path, out: &Path, force: bool) -> Result<()> {
    if !dest.exists() {
        return Ok(());
    }
    if !dest.is_dir() {
        bail!(
            "{}",
            mta(
                "cli-agent-export-dest-not-a-dir",
                &[("path", out.display().to_string().as_str())],
                "destination {$path} exists and is not a directory"
            )
        );
    }
    let mut entries = tokio::fs::read_dir(dest)
        .await
        .with_context(|| format!("failed to read destination {}", dest.display()))?;
    let occupied = entries
        .next_entry()
        .await
        .with_context(|| format!("failed to read destination {}", dest.display()))?
        .is_some();
    if occupied && !force {
        bail!(
            "{}",
            mta(
                "cli-agent-export-dest-not-empty",
                &[("path", out.display().to_string().as_str())],
                "destination {$path} is not empty — pass --force to replace its contents"
            )
        );
    }
    Ok(())
}

/// Swap the staged bundle into place.
fn publish(staging: tempfile::TempDir, dest: &Path, parent: &Path) -> Result<()> {
    // On failure the guard drops and removes the staged tree. On success the
    // rename has already moved it, so the guard is disarmed rather than left to
    // recurse over a path that is now the published bundle.
    swap_into_place(staging.path(), dest, parent)?;
    let _ = staging.keep();
    Ok(())
}

/// Move `staged` onto `dest`, replacing whatever the destination held.
///
/// An existing bundle is moved aside rather than deleted first, so a failed
/// move can put it back: there is no window in which the destination is
/// missing a bundle it had.
fn swap_into_place(staged: &Path, dest: &Path, parent: &Path) -> Result<()> {
    if !dest.exists() {
        return std::fs::rename(staged, dest)
            .with_context(|| format!("failed to move the staged bundle into {}", dest.display()));
    }

    let retired = parent.join(format!("{RETIRED_PREFIX}{}", uuid::Uuid::new_v4()));
    std::fs::rename(dest, &retired).with_context(|| {
        format!(
            "failed to move the existing bundle at {} aside",
            dest.display()
        )
    })?;
    match std::fs::rename(staged, dest) {
        Ok(()) => {
            // The new bundle is published; the old one is now unreferenced.
            // Failing to reap it is untidy, not a failed export.
            std::fs::remove_dir_all(&retired).ok();
            Ok(())
        }
        Err(err) => {
            if std::fs::rename(&retired, dest).is_ok() {
                return Err(err).with_context(|| {
                    format!("failed to move the staged bundle into {}", dest.display())
                });
            }
            let dest_display = dest.display().to_string();
            let retired_display = retired.display().to_string();
            let error = err.to_string();
            bail!(
                "{}",
                mta(
                    "cli-agent-export-restore-failed",
                    &[
                        ("path", dest_display.as_str()),
                        ("retired", retired_display.as_str()),
                        ("error", error.as_str())
                    ],
                    "failed to publish the bundle to {$path} ({$error}), and the previous bundle could not be moved back — it is at {$retired}"
                )
            );
        }
    }
}

async fn write_file(path: &Path, contents: &str) -> Result<()> {
    tokio::fs::write(path, contents)
        .await
        .with_context(|| format!("failed to write {}", path.display()))
}

/// Copy the agent's workspace into the bundle, honoring the plan's memory
/// exclusion. A missing source workspace is not an error: an agent that has
/// never run has nothing on disk yet.
///
/// The workspace is live — the agent that owns it can be writing to it through
/// an ordinary tool call while the export runs — so the walk never re-opens a
/// path by name. The configured root is opened once, and every entry below it
/// is classified *and* read through a handle on the directory that holds it
/// (cap-std, the same beneath/no-follow binding `deliver_file` uses). An entry
/// swapped for a symlink between the classification and the read therefore
/// cannot redirect the copy outside the tree that handle names; a path-based
/// walk would have followed it.
fn copy_workspace(plan: &ExportPlan, dest: &Path) -> Result<WorkspaceCopy> {
    let mut copied = WorkspaceCopy::default();
    if !plan.workspace_source.is_dir() {
        return Ok(copied);
    }
    let source =
        Dir::open_ambient_dir(&plan.workspace_source, ambient_authority()).with_context(|| {
            format!(
                "failed to open workspace {}",
                plan.workspace_source.display()
            )
        })?;
    std::fs::create_dir_all(dest)
        .with_context(|| format!("failed to create {}", dest.display()))?;
    let target = Dir::open_ambient_dir(dest, ambient_authority())
        .with_context(|| format!("failed to open {}", dest.display()))?;
    copy_tree(
        &source,
        &target,
        &PathBuf::new(),
        plan.include_memory,
        &mut copied,
    )?;
    Ok(copied)
}

/// Copy one directory's entries, recursing through child handles.
///
/// Writes are bound the same way as reads: the bundle side is a handle too, so
/// a symlink planted in the staging tree cannot redirect a write out of it.
fn copy_tree(
    source: &Dir,
    dest: &Dir,
    relative: &Path,
    include_memory: bool,
    copied: &mut WorkspaceCopy,
) -> Result<()> {
    let entries = source
        .entries()
        .with_context(|| format!("failed to read {}", rel(relative)))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("failed to read {}", rel(relative)))?;
        let name = entry.file_name();
        let child = relative.join(&name);
        if !agent_bundle::workspace_entry_included(&child, include_memory) {
            continue;
        }

        // `DirEntry::metadata` is a no-follow stat through the directory handle,
        // so it describes the object sitting in *this* directory under that
        // name, not whatever a fresh path lookup would resolve to.
        let file_type = entry
            .metadata()
            .with_context(|| format!("failed to stat {}", rel(&child)))?
            .file_type();
        if file_type.is_symlink() {
            copied.symlinks_skipped += 1;
            continue;
        }
        if !file_type.is_dir() && !file_type.is_file() {
            // Sockets, FIFOs, devices: nothing a bundle can carry.
            continue;
        }

        entry_swap_seam(&child);

        if file_type.is_dir() {
            let child_source = entry
                .open_dir()
                .with_context(|| format!("failed to open {}", rel(&child)))?;
            dest.create_dir(&name)
                .with_context(|| format!("failed to create {} in the bundle", rel(&child)))?;
            let child_dest = dest
                .open_dir(&name)
                .with_context(|| format!("failed to open {} in the bundle", rel(&child)))?;
            copy_tree(&child_source, &child_dest, &child, include_memory, copied)?;
        } else {
            let mut reader = entry
                .open()
                .with_context(|| format!("failed to open {}", rel(&child)))?;
            // The opened handle, not the name, decides what gets copied: an
            // entry that is no longer a regular file is left out rather than
            // read through whatever replaced it.
            let source_metadata = reader
                .metadata()
                .with_context(|| format!("failed to stat {}", rel(&child)))?;
            if !source_metadata.is_file() {
                continue;
            }
            let mut writer = dest
                .open_with(&name, OpenOptions::new().write(true).create_new(true))
                .with_context(|| format!("failed to create {} in the bundle", rel(&child)))?;
            let bytes = std::io::copy(&mut reader, &mut writer)
                .with_context(|| format!("failed to copy {} into the bundle", rel(&child)))?;
            // Carry the mode across, so an executable in the workspace is still
            // executable for whoever imports the bundle.
            writer
                .set_permissions(source_metadata.permissions())
                .with_context(|| format!("failed to set permissions on {}", rel(&child)))?;
            copied.files += 1;
            copied.bytes += bytes;
        }
    }
    Ok(())
}

/// Render a workspace-relative path for an error message. The bundle names
/// entries relative to the workspace root, and so do these.
fn rel(relative: &Path) -> String {
    let shown = relative.display().to_string();
    if shown.is_empty() {
        WORKSPACE_DIR.to_string()
    } else {
        format!("{WORKSPACE_DIR}/{shown}")
    }
}

/// Test seam: runs between an entry's no-follow classification and the
/// handle-bound open of that entry, the interleaving at which a path-based copy
/// could be made to follow a symlink out of the workspace. Compiled away
/// outside tests.
#[cfg(not(test))]
#[inline]
fn entry_swap_seam(_relative: &Path) {}

#[cfg(test)]
fn entry_swap_seam(relative: &Path) {
    tests::run_entry_swap_seam(relative);
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

    /// Sorted names directly under `dir` — used to assert that an export left
    /// no staging or retired directory behind.
    fn entry_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// A swap to perform mid-copy, keyed on the entry's workspace-relative path.
    type Swap = Box<dyn Fn(&Path)>;

    thread_local! {
        /// Swap to perform at [`entry_swap_seam`]. Thread-local, so tests
        /// running in parallel cannot see each other's.
        static ENTRY_SWAP: std::cell::RefCell<Option<Swap>> =
            const { std::cell::RefCell::new(None) };
    }

    pub(super) fn run_entry_swap_seam(relative: &Path) {
        ENTRY_SWAP.with_borrow(|swap| {
            if let Some(swap) = swap.as_ref() {
                swap(relative);
            }
        });
    }

    /// Installs a swap at the copy's check-to-read seam for as long as it is
    /// held, so a replacement race can be reproduced at an exact interleaving.
    struct EntrySwap;

    impl EntrySwap {
        fn install(swap: impl Fn(&Path) + 'static) -> Self {
            ENTRY_SWAP.with_borrow_mut(|slot| *slot = Some(Box::new(swap)));
            Self
        }
    }

    impl Drop for EntrySwap {
        fn drop(&mut self) {
            ENTRY_SWAP.with_borrow_mut(|slot| *slot = None);
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

    #[test]
    fn copy_preserves_the_executable_bit() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let source = tempfile::tempdir().unwrap();
            let dest = tempfile::tempdir().unwrap();
            let script = source.path().join("run.sh");
            write(&script, "#!/bin/sh\n");
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

            copy_workspace(&plan_for(source.path(), false), dest.path()).unwrap();

            let mode = std::fs::metadata(dest.path().join("run.sh"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "mode {mode:o}");
        }
    }

    /// The workspace is writable by the agent being exported, so an entry can be
    /// replaced between the moment the copy classifies it and the moment the
    /// copy reads it. The seam reproduces exactly that interleaving.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_file_replaced_by_an_escaping_symlink_mid_copy_is_not_followed() {
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.txt");
        write(&secret, "host secret");

        let source = tempfile::tempdir().unwrap();
        let entry = source.path().join("notes.md");
        write(&entry, "workspace note");

        let parent = tempfile::tempdir().unwrap();
        let out = parent.path().join("bundle");

        let plan = plan_for(source.path(), false);
        let result = {
            let _swap = EntrySwap::install(move |relative| {
                if relative == Path::new("notes.md") {
                    std::fs::remove_file(&entry).unwrap();
                    std::os::unix::fs::symlink(&secret, &entry).unwrap();
                    // The name now resolves outside the workspace: a copy that
                    // re-opened it by path would read this.
                    assert_eq!(std::fs::read_to_string(&entry).unwrap(), "host secret");
                }
            });
            write_bundle(&plan, &out, false).await
        };

        // Fails closed: the bundle is never published, and the host file the
        // symlink pointed at is nowhere in the output.
        let err = result.unwrap_err();
        assert!(err.to_string().contains("workspace/notes.md"), "{err}");
        assert!(!out.exists());
        assert_eq!(entry_names(parent.path()), Vec::<String>::new());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_directory_replaced_by_an_escaping_symlink_mid_copy_is_not_followed() {
        let outside = tempfile::tempdir().unwrap();
        write(&outside.path().join("secret.txt"), "host secret");

        let source = tempfile::tempdir().unwrap();
        let entry = source.path().join("notes");
        write(&entry.join("plan.md"), "plan");

        let parent = tempfile::tempdir().unwrap();
        let out = parent.path().join("bundle");

        let plan = plan_for(source.path(), false);
        let target = outside.path().to_path_buf();
        let result = {
            let _swap = EntrySwap::install(move |relative| {
                if relative == Path::new("notes") {
                    std::fs::remove_dir_all(&entry).unwrap();
                    std::os::unix::fs::symlink(&target, &entry).unwrap();
                    // A copy that re-walked the name by path would descend here.
                    assert!(entry.join("secret.txt").is_file());
                }
            });
            write_bundle(&plan, &out, false).await
        };

        let err = result.unwrap_err();
        assert!(err.to_string().contains("workspace/notes"), "{err}");
        assert!(!out.exists());
        assert_eq!(entry_names(parent.path()), Vec::<String>::new());
    }

    #[tokio::test]
    async fn export_writes_the_whole_bundle_to_a_fresh_destination() {
        let source = tempfile::tempdir().unwrap();
        write(&source.path().join("IDENTITY.md"), "identity");

        let parent = tempfile::tempdir().unwrap();
        let out = parent.path().join("bundle");

        let copied = write_bundle(&plan_for(source.path(), false), &out, false)
            .await
            .unwrap();

        assert_eq!(copied.files, 1);
        assert!(out.join(CONFIG_FILE).is_file());
        assert!(out.join(MANIFEST_FILE).is_file());
        assert!(out.join(WORKSPACE_DIR).join("IDENTITY.md").is_file());
        // The staging directory was published, not left beside the bundle.
        assert_eq!(entry_names(parent.path()), vec!["bundle".to_string()]);
    }

    #[tokio::test]
    async fn non_empty_destination_is_refused_without_force_and_left_alone() {
        let source = tempfile::tempdir().unwrap();
        write(&source.path().join("IDENTITY.md"), "identity");

        let parent = tempfile::tempdir().unwrap();
        let out = parent.path().join("bundle");
        write(&out.join("config.toml"), "# existing");

        let err = write_bundle(&plan_for(source.path(), false), &out, false)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("--force"), "{err}");
        assert_eq!(entry_names(&out), vec!["config.toml".to_string()]);
        assert_eq!(
            std::fs::read_to_string(out.join("config.toml")).unwrap(),
            "# existing"
        );
        assert_eq!(entry_names(parent.path()), vec!["bundle".to_string()]);
    }

    #[tokio::test]
    async fn force_publishes_a_replacement_rather_than_merging_into_the_old_bundle() {
        let source = tempfile::tempdir().unwrap();
        write(&source.path().join("IDENTITY.md"), "identity");

        let parent = tempfile::tempdir().unwrap();
        let out = parent.path().join("bundle");
        // A previous export of a different shape: entries the new bundle's
        // manifest does not describe must not survive the re-export.
        write(&out.join(CONFIG_FILE), "# stale closure");
        write(&out.join("leftover.toml"), "# stale");
        write(
            &out.join(WORKSPACE_DIR).join("notes/gone.md"),
            "# stale workspace file",
        );

        let copied = write_bundle(&plan_for(source.path(), false), &out, true)
            .await
            .unwrap();

        assert_eq!(copied.files, 1);
        // `leftover.toml` is gone: the bundle was replaced, not merged into.
        assert_eq!(
            entry_names(&out),
            vec![
                CONFIG_FILE.to_string(),
                WORKSPACE_DIR.to_string(),
                MANIFEST_FILE.to_string(),
            ]
        );
        assert!(!out.join(WORKSPACE_DIR).join("notes").exists());
        assert!(out.join(WORKSPACE_DIR).join("IDENTITY.md").is_file());
        assert!(
            std::fs::read_to_string(out.join(CONFIG_FILE))
                .unwrap()
                .contains("agent bundle")
        );
        // Neither the staging nor the retired directory outlived the publish.
        assert_eq!(entry_names(parent.path()), vec!["bundle".to_string()]);
    }

    /// Root ignores mode bits, so probe rather than assume the permission-denied
    /// case is reachable here.
    #[cfg(unix)]
    fn read_permissions_are_enforced(dir: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        let probe = dir.join("probe");
        std::fs::write(&probe, "probe").unwrap();
        std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o000)).unwrap();
        let denied = std::fs::read(&probe).is_err();
        std::fs::remove_file(&probe).ok();
        denied
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_export_removes_its_staging_and_leaves_the_previous_bundle() {
        use std::os::unix::fs::PermissionsExt;

        let probe_dir = tempfile::tempdir().unwrap();
        if !read_permissions_are_enforced(probe_dir.path()) {
            return; // running as root: an unreadable file is still readable
        }

        // A workspace file that cannot be read fails the copy after the config
        // and manifest have already been staged.
        let source = tempfile::tempdir().unwrap();
        let locked = source.path().join("locked.md");
        write(&locked, "unreadable");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

        let parent = tempfile::tempdir().unwrap();
        let out = parent.path().join("bundle");
        write(&out.join(CONFIG_FILE), "# previous export");

        let err = write_bundle(&plan_for(source.path(), false), &out, true)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("workspace/locked.md"), "{err}");
        assert_eq!(entry_names(&out), vec![CONFIG_FILE.to_string()]);
        assert_eq!(
            std::fs::read_to_string(out.join(CONFIG_FILE)).unwrap(),
            "# previous export"
        );
        assert_eq!(entry_names(parent.path()), vec!["bundle".to_string()]);
    }

    #[tokio::test]
    async fn destination_inside_the_workspace_is_refused_before_anything_is_written() {
        let source = tempfile::tempdir().unwrap();
        write(&source.path().join("IDENTITY.md"), "identity");
        let out = source.path().join("exports/bundle");

        let err = write_bundle(&plan_for(source.path(), false), &out, true)
            .await
            .unwrap_err();

        assert!(
            err.to_string().contains("inside the agent workspace"),
            "{err}"
        );
        assert_eq!(entry_names(source.path()), vec!["IDENTITY.md".to_string()]);
    }

    #[tokio::test]
    async fn destination_containing_the_workspace_is_refused_before_anything_is_written() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("agents/researcher/workspace");
        write(&workspace.join("IDENTITY.md"), "identity");

        let err = write_bundle(&plan_for(&workspace, false), root.path(), true)
            .await
            .unwrap_err();

        assert!(
            err.to_string().contains("contains the agent workspace"),
            "{err}"
        );
        assert_eq!(entry_names(root.path()), vec!["agents".to_string()]);
        assert!(workspace.join("IDENTITY.md").is_file());
    }

    #[tokio::test]
    async fn destination_equal_to_the_workspace_is_refused() {
        let source = tempfile::tempdir().unwrap();
        write(&source.path().join("IDENTITY.md"), "identity");

        let err = write_bundle(&plan_for(source.path(), false), source.path(), true)
            .await
            .unwrap_err();

        assert!(
            err.to_string().contains("contains the agent workspace"),
            "{err}"
        );
        assert!(source.path().join("IDENTITY.md").is_file());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlinked_destination_cannot_hide_an_overlap() {
        let source = tempfile::tempdir().unwrap();
        write(&source.path().join("IDENTITY.md"), "identity");

        // `link` resolves into the workspace, so the overlap is only visible
        // once both sides are resolved.
        let links = tempfile::tempdir().unwrap();
        let link = links.path().join("link");
        std::os::unix::fs::symlink(source.path(), &link).unwrap();

        let err = write_bundle(&plan_for(source.path(), false), &link.join("bundle"), true)
            .await
            .unwrap_err();

        assert!(
            err.to_string().contains("inside the agent workspace"),
            "{err}"
        );
        assert_eq!(entry_names(source.path()), vec!["IDENTITY.md".to_string()]);
    }

    #[tokio::test]
    async fn a_file_destination_is_refused() {
        let source = tempfile::tempdir().unwrap();
        write(&source.path().join("IDENTITY.md"), "identity");

        let parent = tempfile::tempdir().unwrap();
        let out = parent.path().join("bundle");
        write(&out, "not a directory");

        let err = write_bundle(&plan_for(source.path(), false), &out, true)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("not a directory"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            "not a directory".to_string()
        );
    }
}
