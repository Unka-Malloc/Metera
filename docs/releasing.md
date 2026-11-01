# Releasing the workspace

Releases run on the generated release train under `.github/workflows/`:
`verify.yml` selects, verifies and packages a candidate; `publish.yml`
holds the only job trusted with the crates.io OIDC identity. Both files are
generated from the release declaration recorded in `.github/release.lock` —
regenerate them instead of editing by hand.

## The two workflows

- **`verify.yml` — the verify train.** Every Friday it rehearses a release
  of the last commit merged into `nightly` before the 23:59 UTC+08:00
  cutoff, bumping by the highest `release:patch|minor|major` label among
  the candidate's pull requests (patch when none is set). A manual dispatch
  releases the head of `nightly` — or the `candidate` input's commit — at
  the exact `version` input or at a `level` bump; `rehearse: true` runs the
  whole pipeline as a dry-run with nothing pushed, tagged or uploaded.
  Either way the job runs `scripts/verify.sh`, computes the release
  version, packages the set and hands the release inputs over as an
  artifact.
- **`publish.yml` — the release side.** It runs the default branch's own
  copy after a `verify` run completes, so a candidate ref can never supply
  the workflow that receives the publishing identity. Its gate binds the
  run to a succeeded `verify.yml` execution on the default branch and
  derives live/dry-run from GitHub-minted metadata: a scheduled train keeps
  its rehearsal mode, a dispatched verify publishes only when it was not a
  rehearsal, and dispatching `publish.yml` itself with a `run_id` is the
  operator's proven intent to publish. The release job recomputes the
  version decision from the bound commit, bumps the workspace, promotes it
  to `stable`, uploads the crates in dependency-first order through
  crates.io Trusted Publishing — no stored secret — then tags `v<next>`,
  moves `release` and merges the tag back into `nightly`.

A candidate identical to the last published release is skipped; a rerun
after a partial upload finishes the same version instead of skipping past
it.

## Versions and order

Crate versions and their internal dependency pins live in the manifests —
[`../Cargo.toml`](../Cargo.toml) for the workspace requirements and each
`crates/*/Cargo.toml` for package versions — which both workflows read
through `cargo metadata`. The publish order is recomputed there rather
than restated here: `collection-store` and `token-usage-detector` have no
internal dependencies, `balance-detector` builds on
`token-usage-detector`, and `agent-statistics` comes last.

## Manual checks (fallback)

When the train itself is unavailable the same gates still run locally:

1. `./scripts/verify.sh` — formatting, clippy with warnings denied, the
   whole workspace test suite.
2. Dependency closure — no git or unversioned path dependencies remain and
   every internal `version = "…"` matches its package version.
3. `cargo package --list` and `cargo publish --dry-run` — the packaged
   surface is the intended one; downstream crates verify against a local
   source replacement holding the already-packaged upstream crates.
4. A consumer project depending on the released versions compiles — proof
   that `agent-statistics` resolves the published `collection-store`.

Local checks prove the candidate; uploading stays with the train's OIDC
identity and never runs from a local checkout.
