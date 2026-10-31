# Releasing the workspace

The four crates are published dependency-first. A version bump is not a
release: the upload waits for the release owner's signing/unlock authorization.

## Publishing order

| Order | Crate | Wave version | Internal dependencies |
| --- | --- | --- | --- |
| 1 | `collection-store` | 0.1.0 | none |
| 2 | `token-usage-detector` | 0.2.1 | none |
| 3 | `balance-detector` | 0.2.0 | `token-usage-detector` |
| 4 | `agent-statistics` | 0.2.0 | `collection-store`, `token-usage-detector` |

Internal dependencies are declared as `{ path = "…", version = "…" }` in the
workspace manifest. Cargo removes the path when packaging, so the published
requirement is the version shown above.

## Four release self-checks

1. **Workspace verification** — `./scripts/verify.sh` runs formatting, clippy
   with warnings denied, and the whole workspace test suite.
2. **Dependency closure** — no `licoup-*`, git, or unversioned path
   dependencies remain; every internal dependency version matches its
   package version; the publish order above is still acyclic.
3. **Package surface** — `cargo package --list` contains the intended files,
   and `cargo publish --dry-run` verifies the package that would be uploaded.
   Downstream crates cannot resolve an unpublished internal version from
   crates.io; verify them against a local source replacement holding the
   already-packaged upstream crates.
4. **Consumer smoke** — a project that depends on the four released versions
   compiles using only the packaged crates. This proves that
   `agent-statistics` consumes the released `collection-store` version and
   that the dependency closure is complete.

## Authorization gate

Checks 3 and 4 can be completed locally, but uploading to crates.io is an
irreversible external action. Run the checks first, then request the release
owner's signing/unlock authorization. Never publish from a dirty working tree
without an explicit release instruction.
