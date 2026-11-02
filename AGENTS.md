# Agent rules for Metera

## Boundary

Two detector crates, one boundary rule each way:

- `token-usage-detector` — maps conversation, session and stream records into
  bound usage observations (push, bounded pull, cursor). A payload never names
  its own source, epoch, scope or measurement.
- `balance-detector` — maps provider account responses into bound credit and
  subscription-quota facts (weekly/monthly remaining share). A payload never
  names the account it describes. It builds on `token-usage-detector`, which
  owns the shared refusal vocabulary (`UsageFailure`, `RecoveryAction`) and
  name helpers; the dependency never runs the other way.

Both crates are the boundary layer, not clients: no network connections, no
ledgers, no charges. Provider-specific acquisition lives in adapters above;
accounting lives in the statistics consumer below.

## Siblings

`acop-usage-source` (and therefore `agent-statistics`' transcript
collection) builds against one sibling checkout:

| Path | Repository | Provides |
| --- | --- | --- |
| `../ACOP` | `Unka-Malloc/ACOP` | `acop` |

## Verify

`scripts/verify.sh`
