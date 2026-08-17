# Agent portability

An agent is not a file. It is a config entry that references providers,
profiles, bundles, and MCP servers, plus a workspace directory on disk. Moving
one to another install means moving both halves, and deciding what should not
move at all.

An **agent bundle** is the portable form:

```text
<bundle>/
├── zeroclaw-agent.toml    — manifest: format version, root alias, provenance,
│                             required secrets, dropped refs, risk flags
├── config.toml            — the config closure this agent needs
└── workspace/             — the agent's workspace tree
```

`config.toml` is a **fragment**, not a whole config. It carries only the
entries the agent references, and it is meant to be read before it is trusted.

## Exporting

<div class="os-tabs-src">

#### sh

```sh
zeroclaw agents export <alias> --out ./my-agent
```

</div>

| Flag | Effect |
| --- | --- |
| `--out <dir>` | Destination bundle directory, created if absent. |
| `--include-memory` | Carry `workspace/memory/` (off by default). |
| `--force` | Replace a destination that already has contents. |

Export is read-only against the install and reports three things: the
capabilities a receiving operator would be accepting, the credentials that were
scrubbed, and the configuration that could not travel.

A bundle is **published, not merged**. The export is built in a staging
directory beside the destination and moved into place in one step, so:

- `--force` *replaces* the destination. A file from an earlier export that the
  new manifest does not describe is gone, rather than left to look like part of
  the bundle.
- A failure anywhere, such as an unreadable workspace file or a full disk,
  leaves the destination exactly as it was. There is no partially written
  bundle to mistake for a complete one.
- A destination that overlaps the agent's workspace is refused before anything
  is created, in both directions: `--out` *inside* the workspace would have the
  copy consume its own output, and an `--out` that *contains* the workspace
  would replace the tree being exported.

### What the closure carries

Starting from `[agents.<alias>]`, the export follows every reference that can
be reconstituted elsewhere:

- the agent's `risk_profile` and `runtime_profile` entries;
- its `skill_bundles`, `knowledge_bundles`, and `mcp_bundles` entries;
- the `[mcp.servers]` entries those bundles actually grant, resolved through
  the same path the runtime uses, so a server removed by a bundle's `exclude`
  is absent from the bundle too;
- every provider entry the agent names (`model_provider`, `classifier_provider`,
  `summary_provider`, `tts_provider`, `transcription_provider`), carried
  **keyless**.

Values equal to the schema default are pruned, so the fragment shows the
choices someone actually made rather than the whole schema.

### What it deliberately leaves behind

Each omission is recorded in the manifest's `dropped` list with a reason, so
nothing disappears silently.

| Dropped | Reason |
| --- | --- |
| `channels` | Names accounts and credentials that exist only on the source install. |
| `delegates`, `workspace.access`, `workspace.read_memory_from` | Name sibling agents that will not exist on the target. |
| `workspace.path` | A source-host absolute path. |
| `a2a` | An outward-facing surface; the agent must be re-published deliberately. |
| `cron_jobs` | Not carried by bundle format 1. |
| `workspace/memory/` | The agent's private history. Opt in with `--include-memory`. |

Symlinks inside the workspace are skipped rather than followed: a link's target
may sit outside the workspace, and it would resolve differently on the
receiving host anyway.

### Credentials

Every field the schema marks secret is scrubbed to an empty string, and its
config path is listed under `required_secrets` in the manifest. The paths are
the ones `zeroclaw config set` accepts, so filling a bundle in is a direct
copy-paste:

```sh
zeroclaw config set providers.models.anthropic.main.api-key
zeroclaw config set mcp.servers.github.env.GITHUB_TOKEN
```

Scrubbing is verified, not assumed: if encrypted config ciphertext survives
into the closure, the export aborts rather than writing the bundle.

### Risk flags

The manifest's `risk_flags` list names each capability in the bundle that
widens the receiving install's trust boundary, bound to the config path that
grants it.

| Flag | Raised by |
| --- | --- |
| `full_autonomy` | `level = "full"`, no per-operation approval gate. |
| `filesystem_escape` | `workspace_only = false`, or `workspace.unrestricted_filesystem = true`. |
| `sandbox_disabled` | `sandbox_enabled = false`. |
| `approval_bypass` | `block_high_risk_commands` or `require_approval_for_medium_risk` turned off. |
| `env_passthrough` | Non-empty `shell_env_passthrough`: host environment variables reach shell subprocesses. |
| `extra_filesystem_roots` | Non-empty `allowed_roots`. |
| `delegation_enabled` | `delegation_policy.mode = "allow"`. |
| `process_spawn` | A stdio MCP server, which starts a local process on the target host. |
| `untrusted_startup_context` | An MCP server's `pinned_resources`: server-controlled text read into the system prompt at startup. |

A bundle from an untrusted source is untrusted input. Read `config.toml` and
the manifest's risk flags before importing one, the same way you would read a
script before running it.

## Importing

Not yet implemented. A bundle is applied by hand today: merge `config.toml`
into your install's config, namespacing any alias that collides with one you
already have, copy `workspace/` into the agent's workspace directory, and
supply the credentials listed in `required_secrets`.

Two rules the future `zeroclaw agents import` will enforce, and that a manual
merge should follow:

- **An import never overwrites an existing entry.** A bundle referencing
  `risk_profiles.default` must not modify *your* `default` profile. Namespace
  the incoming alias, or explicitly point the agent at a local one.
- **The merged config must pass `Config::validate()` before it is saved.** A
  dangling reference is a failed import, not a broken next boot.

## Format version

Bundles carry `format_version = 1`. An exported closure is self-sufficient: it
loads and validates on a fresh install with no other entries present.
