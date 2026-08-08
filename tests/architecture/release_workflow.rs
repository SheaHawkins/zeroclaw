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

fn yaml_block<'a>(document: &'a str, header: &str) -> &'a str {
    let header_indent = header.len() - header.trim_start().len();
    let start = document
        .find(header)
        .unwrap_or_else(|| panic!("workflow is missing YAML block: {header}"));
    let remainder = &document[start + header.len()..];
    let end = remainder
        .split_inclusive('\n')
        .scan(0, |offset, line| {
            let line_start = *offset;
            *offset += line.len();
            Some((line_start, line))
        })
        .find_map(|(offset, line)| {
            let trimmed = line.trim();
            let indent = line.len() - line.trim_start().len();
            (!trimmed.is_empty() && indent <= header_indent).then_some(offset)
        })
        .unwrap_or(remainder.len());
    &remainder[..end]
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
}

#[test]
fn scoop_credential_canary_fails_closed_without_weakening_generic_dry_runs() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let gate = root.join("scripts/release/scoop_credential_gate.sh");

    let run_gate = |dry_run: &str,
                    credential_canary: &str,
                    bucket_repo: Option<&str>,
                    bucket_token: Option<&str>| {
        let mut command = Command::new("bash");
        command
            .arg(&gate)
            .env("DRY_RUN", dry_run)
            .env("CREDENTIAL_CANARY", credential_canary)
            .env_remove("SCOOP_BUCKET_REPO")
            .env_remove("GH_TOKEN");
        if let Some(repo) = bucket_repo {
            command.env("SCOOP_BUCKET_REPO", repo);
        }
        if let Some(token) = bucket_token {
            command.env("GH_TOKEN", token);
        }
        command.output().expect("run Scoop credential gate")
    };

    let generic_dry_run = run_gate("true", "false", None, None);
    assert!(
        generic_dry_run.status.success(),
        "a generic dry run may omit bucket credentials: {}",
        String::from_utf8_lossy(&generic_dry_run.stderr)
    );
    assert_eq!(generic_dry_run.stdout, b"skip\n");

    for (repo, token, missing) in [
        (None, Some("test-token"), "repository"),
        (Some("example/scoop-bucket"), None, "token"),
    ] {
        let canary = run_gate("true", "true", repo, token);
        assert!(
            !canary.status.success(),
            "credential canary must fail when the {missing} is missing"
        );
    }

    let configured_canary = run_gate(
        "true",
        "true",
        Some("example/scoop-bucket"),
        Some("test-token"),
    );
    assert!(
        configured_canary.status.success(),
        "configured credential canary must reach the authorization probe: {}",
        String::from_utf8_lossy(&configured_canary.stderr)
    );
    assert_eq!(configured_canary.stdout, b"probe\n");

    for (repo, token, missing) in [
        (None, Some("test-token"), "repository"),
        (Some("example/scoop-bucket"), None, "token"),
    ] {
        let publish = run_gate("false", "false", repo, token);
        assert!(
            !publish.status.success(),
            "real publish must fail when the {missing} is missing"
        );
    }

    for (dry_run, credential_canary, variable) in [
        ("yes", "false", "DRY_RUN"),
        ("true", "yes", "CREDENTIAL_CANARY"),
    ] {
        let invalid = run_gate(
            dry_run,
            credential_canary,
            Some("example/scoop-bucket"),
            Some("test-token"),
        );
        assert!(
            !invalid.status.success(),
            "invalid {variable} value must fail closed"
        );
    }

    let canary_workflow = workflow("scoop-bucket-canary.yml");
    let canary_job = yaml_block(&canary_workflow, "  rehearse:\n");
    for required in [
        "uses: ./.github/workflows/pub-scoop.yml",
        "dry_run: true",
        "credential_canary: true",
        "SCOOP_BUCKET_TOKEN: ${{ secrets.SCOOP_BUCKET_TOKEN }}",
    ] {
        assert!(
            canary_job.contains(required),
            "Scoop canary is missing fail-closed invariant: {required}"
        );
    }
    assert!(
        !canary_job.contains("secrets: inherit"),
        "Scoop canary must receive only the named bucket token"
    );
    let canary_secrets = yaml_block(canary_job, "    secrets:\n");
    let canary_secret_names = canary_secrets
        .lines()
        .filter(|line| line.starts_with("      ") && !line.starts_with("       "))
        .map(str::trim)
        .collect::<Vec<_>>();
    assert_eq!(
        canary_secret_names,
        ["SCOOP_BUCKET_TOKEN: ${{ secrets.SCOOP_BUCKET_TOKEN }}"],
        "Scoop canary must map exactly the one secret its callee declares"
    );

    let release_workflow = workflow("release-stable-manual.yml");
    let release_scoop_job = yaml_block(&release_workflow, "  scoop:\n");
    for required in [
        "uses: ./.github/workflows/pub-scoop.yml",
        "dry_run: false",
        "SCOOP_BUCKET_TOKEN: ${{ secrets.SCOOP_BUCKET_TOKEN }}",
    ] {
        assert!(
            release_scoop_job.contains(required),
            "real Scoop publisher caller is missing invariant: {required}"
        );
    }
    assert!(
        !release_scoop_job.contains("secrets: inherit"),
        "real Scoop publisher must receive only the named bucket token"
    );
    let release_scoop_secrets = yaml_block(release_scoop_job, "    secrets:\n");
    let release_scoop_secret_names = release_scoop_secrets
        .lines()
        .filter(|line| line.starts_with("      ") && !line.starts_with("       "))
        .map(str::trim)
        .collect::<Vec<_>>();
    assert_eq!(
        release_scoop_secret_names,
        ["SCOOP_BUCKET_TOKEN: ${{ secrets.SCOOP_BUCKET_TOKEN }}"],
        "real Scoop caller must map exactly the one secret its callee declares"
    );

    let publisher_workflow = workflow("pub-scoop.yml");
    let workflow_call = yaml_block(&publisher_workflow, "  workflow_call:\n");
    let workflow_call_secrets = yaml_block(workflow_call, "    secrets:\n");
    let scoop_token = yaml_block(workflow_call_secrets, "      SCOOP_BUCKET_TOKEN:\n");
    assert!(
        scoop_token.contains("required: true"),
        "reusable Scoop publisher must require its declared bucket token"
    );
    let declared_secrets = workflow_call_secrets
        .lines()
        .filter(|line| line.starts_with("      ") && !line.starts_with("       "))
        .collect::<Vec<_>>();
    assert_eq!(
        declared_secrets,
        ["      SCOOP_BUCKET_TOKEN:"],
        "reusable Scoop publisher must declare exactly one secret"
    );

    let publisher_job = yaml_block(&publisher_workflow, "  publish-scoop:\n");
    let canary_env = "CREDENTIAL_CANARY: ${{ inputs.credential_canary }}";
    assert_eq!(
        publisher_job.matches(canary_env).count(),
        1,
        "publisher job must map credential_canary into the tested gate exactly once"
    );
    assert_eq!(
        publisher_workflow.matches(canary_env).count(),
        1,
        "credential_canary env mapping must not drift outside the publisher job"
    );
    assert!(
        publisher_job.contains("gate_result=\"$(bash scripts/release/scoop_credential_gate.sh)\""),
        "Scoop publisher must enforce the tested credential gate"
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
