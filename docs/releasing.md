# Releasing the workspace

Releases run on the generated release train under `.github/workflows/`:
`verify.yml` selects, verifies and packages a candidate; `publish.yml`
holds the only job trusted with the crates.io OIDC identity. Both files are
generated from the release declaration recorded in `.github/release.lock` —
regenerate them instead of editing by hand.

## The two workflows

- **`verify.yml` — the verify train.** Every Friday it rehearses a release
  of the last commit merged into the default branch before the 23:59
  UTC+08:00 cutoff, bumping by the highest `release:patch|minor|major`
  label among the candidate's pull requests (patch when none is set). A
  manual dispatch releases the head of the default branch — or the
  `candidate` input's commit — at the exact `version` input or at a
  `level` bump; `rehearse: true` runs the whole pipeline as a dry-run with
  nothing pushed, tagged or uploaded.
  Either way the job runs `scripts/verify.sh`, computes the release
  version, packages the set and hands the release inputs over as an
  artifact.
- **`publish.yml` — the release side.** `workflow_run` on the completed
  `verify` train is its only trigger — there is deliberately no
  `workflow_dispatch` and no tag path on the OIDC file — so the workflow
  that executes under the publishing identity is always the default
  branch's own copy. Its gate binds the run to a succeeded `verify.yml`
  execution on the default branch and derives live/dry-run and the
  release kind from metadata GitHub minted — the run's event and the
  signed verify job name (`sig=1`/`sched=`/`rehearse`/`version`/`level`/
  `cand=`), never from the `release-inputs` artifact, whose only fields
  are `sha` and `release`. The release job runs in the `release`
  environment, recomputes the version decision from the bound commit,
  bumps the workspace, promotes it to `stable`, uploads the crates in
  dependency-first order through crates.io Trusted Publishing — no stored
  secret — then tags `v<next>`, moves `release` and merges the tag back
  into the bound source branch.

A candidate identical to the last published release is skipped; a rerun
after a partial upload finishes the same version instead of skipping past
it.

## Releasing manually

Dispatch `verify.yml` on the default branch with `rehearse: false`,
optionally naming `version`, `level` or `candidate`. A live
(non-rehearsal) run flows into `publish.yml` and still meets the
`release` environment's approval; a scheduled train stays a rehearsal
and never publishes. There is no `run_id` input and no way to dispatch
`publish.yml` directly — the operator's proven intent to publish is the
verify dispatch plus the environment approval.

## One-time infrastructure

Two settings live outside the generated files and are created once per
repository:

- A `release` environment with required reviewers and a
  deployment-branch rule allowing only the default branch. The
  environment — not the in-file ref check, which stays as a second
  lock — is what confines a dispatch's reach.
- crates.io Trusted Publishing for each published crate bound to that
  `release` environment and to `publish.yml` as the workflow filename —
  registries mint the OIDC token for that file, which is why the
  publishing job lives there and not in `verify.yml`. No stored
  publishing credential exists; the only optional secret is
  `RELEASE_ENV_READ_TOKEN`, a read-scoped token the environment
  preflight step falls back to when `github.token` cannot read
  `/environments` — without it an unreadable answer simply counts as
  'unverified' rather than blocked.

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
