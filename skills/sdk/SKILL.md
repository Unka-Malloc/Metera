---
name: agent-statistics-sdk
description: Embed the statistics detectors and registry in a Rust host — bound facts, account bindings, boundary discipline.
---

# agent-statistics SDK mode

Embed `token-usage-detector`, `balance-detector` and `agent-statistics`.
No sidecar exists — the detectors are boundary layers, not clients.

## When to use

- The host already acquires provider payloads (tokens, quotas, balances)
  and wants validated, bound facts; token usage can also settle into the
  usage journal below.
- Provider-specific fetching stays in the host's adapters — the detectors
  never open a connection.

## Canonical calls

```rust
use balance_detector::{
    AccountBinding, BalanceFact, BalanceMapping, QuotaMapping, QuotaWindow,
};

// The host issued the query, so it already knows the account identity.
let binding = AccountBinding::new(provider_ref, account_ref, account_epoch)?;

// Direct path: bind facts the host mapped itself. The list must be non-empty.
let claims: Vec<BalanceFact> = host_facts;
let _bound = binding.bind(claims, observed_at.clone())?;

// Declarative path: map a raw response through a declared shape.
let mapping = BalanceMapping {
    quotas: vec![QuotaMapping {
        remaining_percent: Some("usage.weekly.remainingPercent".to_owned()),
        resets_at: Some("usage.weekly.resetsAt".to_owned()),
        ..QuotaMapping::new(QuotaWindow::Weekly)
    }],
    ..BalanceMapping::default()
};
match mapping.normalize(&payload, &binding, observed_at)? {
    Some(bound) => {
        // `bound.facts` is the non-empty, ≤32-fact list; `bound.observed_at`
        // is the host clock, not a timestamp from the payload.
    }
    None => {
        // The response carried none of the mapped fields. That is no claim,
        // not a zero balance; the host decides whether silence is retryable.
    }
}
```

## Rules that keep the contract

- **Host issues identity**: `provider_ref`/`account_ref`/`account_epoch` are
  namespaced strings the host derives from custody (JWT claims, keyring
  records) — never from the payload. Self-asserted account fields in the
  payload are rejected recursively.
- Facts are verbatim: no rounding, no unit conversion, no arithmetic —
  `Quota.remaining_percent` is the provider's own text.
- A payload carrying none of the mapped fields produces no facts
  (`normalize` returns `Ok(None)`); an empty fact list refuses to bind
  (`bind` returns an error).
- `agent-statistics` owns accounting below the boundary; the detectors
  own validation above it — don't let either grow the other's job.
