//! Architecture gates for release-workflow artifact identity.
//!
//! The macOS desktop job must notarize, staple, validate, and upload the same
//! DMG. Discovering the file independently in multiple steps can notarize one
//! image while publishing another.

use std::fs;
use std::path::Path;
use std::process::Command;

fn workflow(name: &str) -> String {
    let workflow_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".github/workflows")
        .join(name);
    fs::read_to_string(&workflow_path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", workflow_path.display()))
}

#[test]
fn macos_desktop_release_notarizes_published_dmg() {
    let workflow_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/workflows/release-stable-manual.yml");
    let workflow = fs::read_to_string(&workflow_path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", workflow_path.display()));
    let macos_job = workflow
        .split_once("  build-desktop:\n")
        .and_then(|(_, remainder)| remainder.split_once("  # New desktop platforms."))
        .map(|(job, _)| job)
        .expect("release workflow must contain the macOS desktop build job");

    assert_eq!(
        macos_job.matches("MACOS_DMG_PATH:").count(),
        1,
        "the published DMG path must have exactly one source of truth"
    );
    for required in [
        "MACOS_DMG_PATH: desktop-assets/ZeroClaw.dmg",
        "dmg_dir=\"target/universal-apple-darwin/release/bundle/dmg\"",
        "dmg_candidates=(\"$dmg_dir\"/*.dmg)",
        "\"${#dmg_candidates[@]}\" -ne 1",
        "mv \"${dmg_candidates[0]}\" \"$MACOS_DMG_PATH\"",
        "notarytool submit \"$MACOS_DMG_PATH\"",
        "stapler staple \"$MACOS_DMG_PATH\"",
        "stapler validate \"$MACOS_DMG_PATH\"",
        "${{ env.MACOS_DMG_PATH }}",
    ] {
        assert!(
            macos_job.contains(required),
            "macOS desktop job is missing release invariant: {required}"
        );
    }

    assert!(
        !macos_job.contains("find target -name '*.dmg'"),
        "the macOS desktop job must not rediscover DMGs from the whole target tree"
    );

    let positions = [
        "mv \"${dmg_candidates[0]}\" \"$MACOS_DMG_PATH\"",
        "notarytool submit \"$MACOS_DMG_PATH\"",
        "stapler staple \"$MACOS_DMG_PATH\"",
        "stapler validate \"$MACOS_DMG_PATH\"",
        "uses: actions/upload-artifact@",
    ]
    .map(|needle| {
        macos_job
            .find(needle)
            .unwrap_or_else(|| panic!("macOS desktop job is missing ordered step: {needle}"))
    });
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "the final DMG must be prepared, notarized, stapled, validated, then uploaded"
    );
}

#[test]
fn package_publishers_use_canonical_sources_and_scoped_credentials() {
    let release = workflow("release-stable-manual.yml");
    assert!(
        !release.contains("pub-homebrew-core.yml"),
        "Homebrew Core is updated by its official autobump service, not a duplicate publisher"
    );
    assert!(
        !Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".github/workflows/pub-homebrew-core.yml")
            .exists(),
        "the redundant project-owned Homebrew publisher must stay retired"
    );

    let scoop = workflow("pub-scoop.yml");
    for required in [
        "SCOOP_BUCKET_TOKEN",
        "dist/scoop/zeroclaw.json",
        "push --dry-run origin HEAD",
        "Contents: Read and write",
        ".architecture[\"64bit\"].url = $url",
        ".architecture[\"64bit\"].hash = $hash",
    ] {
        assert!(
            scoop.contains(required),
            "Scoop publisher is missing packaging invariant: {required}"
        );
    }
    for forbidden in [
        "gh api \"repos/${SCOOP_BUCKET_REPO}\" --jq '.permissions.push'",
        "cat > \"$manifest_file\" <<MANIFEST",
    ] {
        assert!(
            !scoop.contains(forbidden),
            "Scoop publisher must not contain duplicate or heuristic path: {forbidden}"
        );
    }

    assert!(
        scoop.contains(
            "bash scripts/release/scoop_metadata.sh dist/scoop/zeroclaw.json \"$version\""
        ),
        "pub-scoop.yml must materialize publisher metadata from the canonical manifest"
    );
    assert!(
        !scoop.contains("https://github.com/${GITHUB_REPOSITORY}/releases/download/"),
        "pub-scoop.yml must not rebuild a release URL independently of the canonical manifest"
    );

    let aur = workflow("pub-aur.yml");
    assert!(
        !aur.contains("ssh -T -o"),
        "AUR clone/push is the authoritative authentication check"
    );
    for required in [
        "group: aur-publish-${{ github.repository }}-${{ inputs.dry_run }}",
        "timeout-minutes: 20",
        "if: inputs.dry_run == false\n        timeout-minutes: 12",
        "if (( attempt_status == 2 )); then",
        "stopped because the freshly cloned package failed the monotonic version guard",
    ] {
        assert!(
            aur.contains(required),
            "AUR publisher is missing release-safety invariant: {required}"
        );
    }

    let workflow_call_inputs = aur
        .split_once("  workflow_call:\n")
        .and_then(|(_, remainder)| remainder.split_once("  workflow_dispatch:\n"))
        .map(|(block, _)| block)
        .expect("AUR publisher must define workflow_call before workflow_dispatch");
    assert!(
        !workflow_call_inputs.contains("allow_downgrade"),
        "automated reusable callers must not be able to authorize an AUR downgrade"
    );
    let manual_inputs = aur
        .split_once("  workflow_dispatch:\n")
        .and_then(|(_, remainder)| remainder.split_once("\nconcurrency:\n"))
        .map(|(block, _)| block)
        .expect("AUR publisher must define manual dispatch inputs");
    assert!(
        manual_inputs.contains("allow_downgrade:"),
        "manual recovery must expose an explicit downgrade override"
    );
    let downgrade_input = manual_inputs
        .split_once("allow_downgrade:")
        .map(|(_, block)| block)
        .expect("manual dispatch must expose allow_downgrade");
    assert!(
        downgrade_input.contains("default: false"),
        "manual downgrade authorization must default to false"
    );

    let guard_call = r#"guard_command=(bash "$GITHUB_WORKSPACE/scripts/release/aur_version_guard.sh")
            if [[ "$ALLOW_DOWNGRADE" == "true" ]]; then
              guard_command+=(--allow-downgrade)
            fi
            guard_command+=( \
              "$SRCINFO_FILE" "$work_dir/.SRCINFO" \
              "$PKGBUILD_FILE" "$work_dir/PKGBUILD" \
            )
            if ! "${guard_command[@]}"; then
              return 2
            fi"#;
    assert!(
        aur.contains(guard_call),
        "AUR publisher must map the complete package guard to a permanent failure"
    );
    assert_eq!(
        aur.matches("scripts/release/aur_version_guard.sh").count(),
        1,
        "the AUR guard invocation must have one unambiguous source of truth"
    );

    let clone_position = aur
        .find("git clone --quiet ssh://aur@aur.archlinux.org/zeroclawlabs.git")
        .expect("AUR publisher must clone the authoritative package state");
    let guard_position = aur
        .find(guard_call)
        .expect("AUR publisher must enforce monotonic versions");
    let overwrite_position = aur
        .find("cp \"$PKGBUILD_FILE\" \"$work_dir/PKGBUILD\"")
        .expect("AUR publisher must update PKGBUILD");
    assert!(
        clone_position < guard_position && guard_position < overwrite_position,
        "the AUR monotonic guard must inspect each fresh clone before package metadata is overwritten"
    );

    let freshness = workflow("aur-freshness-check.yml");
    assert!(
        freshness.contains(
            "aur_version=\"${aur_full%%-*}\"\n          aur_version=\"${aur_version#*:}\""
        ),
        "AUR freshness must remove pkgrel and epoch before comparing pkgver to the release"
    );
}

#[test]
fn aur_publisher_rejects_stale_release_downgrades() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let guard_script = root.join("scripts/release/aur_version_guard.sh");
    let temp = tempfile::tempdir().expect("create temporary AUR package directory");
    let target_srcinfo = temp.path().join("target.SRCINFO");
    let current_srcinfo = temp.path().join("current.SRCINFO");
    let target_pkgbuild = temp.path().join("target.PKGBUILD");
    let current_pkgbuild = temp.path().join("current.PKGBUILD");

    let srcinfo = |epoch: Option<u32>, version: &str, release: &str| {
        let epoch = epoch.map_or_else(String::new, |value| format!("epoch = {value}\n"));
        format!(
            "pkgbase = zeroclawlabs\n{epoch}pkgver = {version}\npkgrel = {release}\npkgname = zeroclawlabs\n"
        )
    };
    let pkgbuild = |epoch: Option<u32>, version: &str, release: &str| {
        let epoch = epoch.map_or_else(String::new, |value| format!("epoch={value}\n"));
        format!("pkgname=zeroclawlabs\n{epoch}pkgver={version}\npkgrel={release}\n")
    };

    let run_guard = |target_metadata: &str,
                     current_metadata: &str,
                     target_build: &str,
                     current_build: &str,
                     allow_downgrade: bool| {
        fs::write(&target_srcinfo, target_metadata).expect("write target AUR .SRCINFO");
        fs::write(&current_srcinfo, current_metadata).expect("write current AUR .SRCINFO");
        fs::write(&target_pkgbuild, target_build).expect("write target AUR PKGBUILD");
        fs::write(&current_pkgbuild, current_build).expect("write current AUR PKGBUILD");
        let mut command = Command::new("bash");
        command.arg(&guard_script);
        if allow_downgrade {
            command.arg("--allow-downgrade");
        }
        command
            .arg(&target_srcinfo)
            .arg(&current_srcinfo)
            .arg(&target_pkgbuild)
            .arg(&current_pkgbuild)
            .output()
            .expect("run AUR monotonic package guard")
    };

    let same_build = pkgbuild(None, "1.2.3", "1");
    let equal = srcinfo(None, "1.2.3", "1");
    let output = run_guard(&equal, &equal, &same_build, &same_build, false);
    assert!(
        output.status.success(),
        "an unchanged package must be idempotent: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    for (target, current) in [("1.2.4", "1.2.3"), ("1.10.0", "1.9.9")] {
        let output = run_guard(
            &srcinfo(None, target, "1"),
            &srcinfo(None, current, "1"),
            &pkgbuild(None, target, "1"),
            &pkgbuild(None, current, "1"),
            false,
        );
        assert!(
            output.status.success(),
            "target {target} should be allowed over {current}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let older = srcinfo(None, "1.9.9", "1");
    let newer = srcinfo(None, "1.10.0", "1");
    let older_build = pkgbuild(None, "1.9.9", "1");
    let newer_build = pkgbuild(None, "1.10.0", "1");
    let output = run_guard(&older, &newer, &older_build, &newer_build, false);
    assert_eq!(
        output.status.code(),
        Some(3),
        "an older workflow must return the dedicated downgrade status"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Refusing AUR downgrade"),
        "downgrade rejection must explain why publishing stopped"
    );

    let output = run_guard(&older, &newer, &older_build, &newer_build, true);
    assert!(
        output.status.success()
            && String::from_utf8_lossy(&output.stderr).contains("Manual AUR downgrade override"),
        "an explicit manual override must permit a deliberate rollback"
    );

    let output = run_guard(
        &srcinfo(None, "1.2.3", "1"),
        &srcinfo(None, "1.2.3", "2"),
        &pkgbuild(None, "1.2.3", "1"),
        &pkgbuild(None, "1.2.3", "2"),
        false,
    );
    assert_eq!(
        output.status.code(),
        Some(3),
        "pkgrel must participate in monotonic package ordering"
    );

    let output = run_guard(
        &srcinfo(None, "2.0.0", "1"),
        &srcinfo(Some(1), "1.0.0", "1"),
        &pkgbuild(None, "2.0.0", "1"),
        &pkgbuild(Some(1), "1.0.0", "1"),
        false,
    );
    assert_eq!(
        output.status.code(),
        Some(3),
        "epoch must take precedence over pkgver"
    );

    let changed_build = format!("{same_build}# changed metadata\n");
    let output = run_guard(&equal, &equal, &changed_build, &same_build, false);
    assert_eq!(
        output.status.code(),
        Some(4),
        "different package files must not reuse an existing version tuple"
    );

    let output = run_guard(&equal, &equal, &changed_build, &same_build, true);
    assert_eq!(
        output.status.code(),
        Some(4),
        "manual downgrade authorization must not permit same-version rewrites"
    );

    let malformed = srcinfo(None, "not-a-version", "1");
    let output = run_guard(&equal, &malformed, &same_build, &same_build, false);
    assert_eq!(
        output.status.code(),
        Some(2),
        "unparseable current AUR state must return a hard validation failure"
    );
    let output = run_guard(&equal, &malformed, &same_build, &same_build, true);
    assert_eq!(
        output.status.code(),
        Some(2),
        "manual downgrade authorization must not permit malformed AUR state"
    );

    let extra_equals = equal.replace("pkgver = 1.2.3", "pkgver = 1.2.3 = junk");
    let output = run_guard(&equal, &extra_equals, &same_build, &same_build, false);
    assert_eq!(
        output.status.code(),
        Some(2),
        "SRCINFO values with trailing equals data must not be truncated"
    );

    let malformed_build = same_build.replace("pkgver=1.2.3", "pkgver=1.2.3=junk");
    let output = run_guard(&equal, &equal, &same_build, &malformed_build, false);
    assert_eq!(
        output.status.code(),
        Some(2),
        "PKGBUILD values with trailing equals data must not be truncated"
    );

    let duplicate = format!("{equal}pkgver = 9.9.9\n");
    let output = run_guard(&equal, &duplicate, &same_build, &same_build, false);
    assert_eq!(
        output.status.code(),
        Some(2),
        "multiple pkgver fields must fail closed"
    );

    let mismatched_build = pkgbuild(None, "1.2.3", "2");
    let output = run_guard(&equal, &equal, &mismatched_build, &same_build, false);
    assert_eq!(
        output.status.code(),
        Some(2),
        "generated PKGBUILD and .SRCINFO version tuples must agree"
    );

    fs::write(&target_srcinfo, &equal).expect("restore target AUR .SRCINFO");
    fs::write(&target_pkgbuild, &same_build).expect("restore target AUR PKGBUILD");
    fs::remove_file(&current_srcinfo).expect("remove current AUR .SRCINFO");
    fs::remove_file(&current_pkgbuild).expect("remove current AUR PKGBUILD");
    let output = Command::new("bash")
        .arg(&guard_script)
        .arg(&target_srcinfo)
        .arg(&current_srcinfo)
        .arg(&target_pkgbuild)
        .arg(&current_pkgbuild)
        .output()
        .expect("run AUR guard with missing current metadata");
    assert_eq!(
        output.status.code(),
        Some(0),
        "a completely empty cloned package must permit first publish"
    );

    fs::write(&current_srcinfo, &equal).expect("restore only current AUR .SRCINFO");
    let output = Command::new("bash")
        .arg(&guard_script)
        .arg(&target_srcinfo)
        .arg(&current_srcinfo)
        .arg(&target_pkgbuild)
        .arg(&current_pkgbuild)
        .output()
        .expect("run AUR guard with partial current metadata");
    assert_eq!(
        output.status.code(),
        Some(2),
        "a partially populated cloned package must fail closed"
    );
}

#[test]
fn scoop_publisher_metadata_follows_canonical_url_template() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let metadata_script = root.join("scripts/release/scoop_metadata.sh");
    let script = fs::read_to_string(&metadata_script)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", metadata_script.display()));
    assert!(
        !script.contains("eval "),
        "canonical Scoop URL templates must never be evaluated as shell code"
    );

    let temp = tempfile::tempdir().expect("create temporary Scoop manifest directory");
    let manifest_path = temp.path().join("zeroclaw.json");
    fs::write(
        &manifest_path,
        r#"{
  "autoupdate": {
    "architecture": {
      "64bit": {
        "url": "https://downloads.example.test/renamed/repository/releases/v$version/zeroclaw-renamed.zip"
      }
    }
  }
}"#,
    )
    .expect("write temporary Scoop manifest");

    let output = Command::new("bash")
        .arg(&metadata_script)
        .arg(&manifest_path)
        .arg("1.2.3")
        .output()
        .expect("run Scoop metadata materializer");
    assert!(
        output.status.success(),
        "Scoop metadata materializer failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse Scoop publisher metadata");

    assert_eq!(
        metadata["zip_url"],
        "https://downloads.example.test/renamed/repository/releases/v1.2.3/zeroclaw-renamed.zip"
    );
    assert_eq!(metadata["asset_name"], "zeroclaw-renamed.zip");
    assert_eq!(
        metadata["sums_url"],
        "https://downloads.example.test/renamed/repository/releases/v1.2.3/SHA256SUMS"
    );

    let output = Command::new("bash")
        .arg(&metadata_script)
        .arg(&manifest_path)
        .arg("v1.2.3")
        .output()
        .expect("run Scoop version validation");
    assert!(
        !output.status.success(),
        "metadata materializer must independently validate the release version"
    );

    for invalid_template in [
        "",
        "https://downloads.example.test/releases/v$version/\nzeroclaw.zip",
        "https://downloads.example.test/releases/latest/zeroclaw.zip",
    ] {
        let invalid_manifest = serde_json::json!({
            "autoupdate": {
                "architecture": {
                    "64bit": {"url": invalid_template}
                }
            }
        });
        fs::write(
            &manifest_path,
            serde_json::to_vec(&invalid_manifest).expect("serialize invalid Scoop manifest"),
        )
        .expect("write invalid Scoop manifest");
        let output = Command::new("bash")
            .arg(&metadata_script)
            .arg(&manifest_path)
            .arg("1.2.3")
            .output()
            .expect("run Scoop metadata validation");
        assert!(
            !output.status.success(),
            "invalid canonical template must fail closed: {invalid_template:?}"
        );
    }
}
