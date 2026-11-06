# Integration modes

There is no sidecar: the crates are embedded in the host process. Provider
acquisition, credentials, retries and account custody stay in the host. The
detectors validate and bind what the host acquired; `agent-statistics` owns the
usage journal, aggregation and retained reports.

## SDK — embed the crates

```toml
[dependencies]
token-usage-detector = "0.2"   # records → bound usage observations
balance-detector = "0.2"       # provider responses → bound balance facts
agent-statistics = "0.3"       # usage journal, aggregation and retained reports
```

`collection-store` is the storage primitive underneath `agent-statistics`; depend
on it directly only when the host also needs bounded collection documents.

`token-usage-detector::agent::{TokenUsage, from_aliases, from_opencode_tokens}`
provides raw Agent counters, alias parsing and nested OpenCode token mapping
without depending on `agent-statistics`, Acativa or SQLite. A4AA transports
acquire and forward the counters; protocol acquisition, framing and quota-error
classification remain in A4AA.
`TokenUsage` is not a `BoundObservation`: the host must map the counters into
usage observations and bind them through the existing `SourceBinding` before
accounting. Source identity, epoch, scope and measurement authority stay with
the host.

### Statistics host and scanning

Install the product answers once at process start, then call `scan` and
`report`. `scan` reads each supported agent through the host's
`summarize_agent_history` answer, settles the readings into the base usage
journal composed by `source_host`, and persists the retained report.
`report` returns the retained reports plus the current ledger projection.

```rust
use agent_statistics::registry::EmptyModelRegistry;
use agent_statistics::{StatisticsHost, install_host, report, scan};
use serde_json::json;
use std::sync::Arc;

fn statistics(state_root: &str) -> Result<(), Box<dyn std::error::Error>> {
    install_host(StatisticsHost {
        supported_agents: || Vec::new(), // the host's AgentDef catalog
        summarize_agent_history: |_def, _params, _window, _warnings| {
            // The host's reader for this agent: Result<HistoryUsageSummary>.
            unimplemented!("host history reader")
        },
        target_status: |_params, _warnings| Default::default(),
        model_registry_snapshot: |_state_root| Arc::new(EmptyModelRegistry),
    })
    .expect("the host installs once per process");

    let params = json!({"stateRoot": state_root, "days": 30});
    let _measured = scan(&params)?; // measures, settles into the journal, retains the report
    let _retained = report(&params)?; // reads retained reports and the ledger projection
    Ok(())
}
```

`scan` composes the journal itself, so a host that only uses the native history
readers does not open it separately. A journal that cannot be opened is reported
as a warning on the scan and as `available: false` on the report; it never
invents an empty ledger.

### Usage delivered by a source host

Pushed batches, pulled pages and resumed cursors settle through the same
journal. Compose the host, issue the admission, then deliver on it:

```rust
use agent_statistics::journal::SettlementEligibility;
use agent_statistics::source_host;
use serde_json::json;
use token_usage_detector::SourceBinding;
use token_usage_detector::collection::PublishBatch;

fn deliver(state_root: &str, batch: &PublishBatch) -> Result<(), Box<dyn std::error::Error>> {
    let params = json!({"stateRoot": state_root});
    source_host::install(&params)?;
    let journal = source_host::source_host(&params)?;
    let mut journal = journal.lock().expect("journal lock");

    let admission = journal.admit_source(
        SourceBinding::new(
            "source:example.analytics#1",
            "example.analytics",
            "instance-1",
            1,
            "epoch-1",
            ["scope-1"],
        )?,
        SettlementEligibility::SettlementEligible,
    );
    journal.publish(&admission, batch)?;
    Ok(())
}
```

`admit_page` accepts a pulled page against the request `resume_request` issued;
`admit_observation` accepts one observation the host already bound. All three
paths land in the same ledger as the scan's native readings.

### Balance facts are not statistics input

`balance-detector` answers a different question: how much credit or quota
remains on an account. `agent-statistics` does not consume `BoundBalance`.
The host binds and maps the provider response, then hands the facts to its own
account surface or downstream accounting; do not route them through
`scan`/`report`.

## Boundary rules the consumer relies on

- A payload **never names its own account, source, epoch or scope** — the
  host binds every fact to a custody-issued `AccountBinding` or
  `SourceBinding` before it counts. Payload self-assertion fields are rejected
  at normalization.
- Detectors never round, convert or fetch: facts carry the provider's
  verbatim values; mapping is declared per source (`BalanceMapping`, token
  mapping tables), never improvised.
- Missing data stays absent. A balance mapping that finds none of its declared
  fields returns `Ok(None)`, never a zero; a usage count the producer does not
  know is `Quality::Unknown`, never a number.

## Skills

- `skills/sdk/` — embedding guidance (bindings, facts, registry, boundary
  discipline).
