# Metera

The agent statistics family: detector crates that validate and bind the facts,
and the journal engine that measures and keeps them.

| Crate | Question it answers |
| --- | --- |
| `token-usage-detector` | What did the work cost? Maps conversation, session and stream records into bound usage observations through the push, bounded-pull and cursor collection protocol, with the source identity, epoch, scope and measurement attached by the host — never by the payload. |
| `balance-detector` | How much remains? Maps provider account responses into bound credit and subscription-quota facts (remaining amounts, weekly/monthly remaining percentages, reset times), with the account bound by the host and provider-specific fetching behind the `BalanceSource` port. |
| `collection-store` | Where does local state live? Bounded, atomically written JSON collection documents under the portable client-state directory. |
| `agent-statistics` | What does the history say? The usage journal, incremental projection, windowing, attribution, retained-report persistence, request counters and their Prometheus exposition; product knowledge arrives through the `StatisticsHost` port. |

The detector crates are boundary layers: they open no connections, store no
ledgers and compute no charges. Provider-specific acquisition lives in
adapters; `agent-statistics` is the accounting side the host composes.

`token-usage-detector::agent` owns raw Agent token counters, alias parsing and
nested OpenCode token mapping; `token-usage-detector::provider` maps the
usage records provider APIs return — flat or nested, across spelling variants
— into the same counters while refusing transport-bound fields. A4AA
transports acquire protocol records and forward these counters through the
shared detector contract. Raw counters are not bound accounting observations:
the host must map them into usage observations and bind them through
`SourceBinding` before accounting.

## License

Apache-2.0
