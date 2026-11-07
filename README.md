# Metera

The agent statistics family: detector crates that validate and bind the facts,
and the journal engine that measures and keeps them.

| Crate | Question it answers |
| --- | --- |
| `token-usage-detector` | What did the work cost? Maps conversation, session and stream records into bound usage observations through the push, bounded-pull and cursor collection protocol, with the source identity, epoch, scope and measurement attached by the host — never by the payload. |
| `balance-detector` | How much remains? Maps provider account responses into bound credit and subscription-quota facts (remaining amounts, weekly/monthly remaining percentages, reset times), with the account bound by the host and provider-specific fetching behind the `BalanceSource` port. |
| `collection-store` | Where does local state live? Bounded, atomically written JSON collection documents under the portable client-state directory. |
| `agent-statistics` | What does the history say? The usage journal, incremental projection, windowing, attribution, retained-report persistence, request counters and their Prometheus exposition; product knowledge arrives through the `StatisticsHost` port. |
| `acop-usage-source` | What did an agent spend? Read-only agent-side token counters projected from ACOP transcripts and stores into usage payloads for a `SourceBinding`; the caller supplies epoch and scope. |
| `metera-sdk` | How does a host run the shared kernel? Rust host SDK that loads the bundled Metera WebAssembly usage kernel and calls its `metera.kernel.v1` store operations. |
| `metera-wasm` | The WebAssembly usage kernel itself (not published). `scripts/build-kernel.sh` builds it reproducibly into the tracked `crates/metera-sdk/wasm/metera.wasm`; `sdk/go` is the Go host for the same kernel. |

The detector crates are boundary layers: they open no connections, store no
ledgers and compute no charges. Provider-specific acquisition lives in
adapters; `agent-statistics` is the accounting side the host composes.
`balance-detector` builds on `token-usage-detector` — the usage vocabulary is
the shared base, and the dependency never runs the other way.

`token-usage-detector::agent` owns raw Agent token counters, alias parsing and
nested OpenCode token mapping; `token-usage-detector::provider` maps the
usage records provider APIs return — flat or nested, across spelling variants
— into the same counters while refusing transport-bound fields. A4AA
transports acquire protocol records and forward these counters through the
shared detector contract. Raw counters are not bound accounting observations:
the host must map them into usage observations and bind them through
`SourceBinding` before accounting.

## Verify

`scripts/verify.sh` — fmt, the kernel build, clippy and tests across the
workspace, then `go vet` and `go test` for `sdk/go`. The kernel is built in a
pinned linux/amd64 Docker image and must match the tracked
`crates/metera-sdk/wasm/metera.wasm` byte for byte, so verification requires
Docker (with amd64 emulation on ARM hosts).

Every dependency, including `acop` and `acop-usage-core` from
[Agent-Context-Operators](https://github.com/Unka-Malloc/Agent-Context-Operators),
resolves from crates.io; a fresh clone builds on its own.

## License

Apache-2.0
