# Agent-side read path: through ACOP

`acop-usage-source` is the acquisition adapter for agent stores. It
reads sessions **through ACOP** — the codec owns session discovery
(`store::scan`, `roots_of`) and declares the `TranscriptUsage` spec:
which history root holds counters, which record shape carries them and
which fields mean input, cached input, output and total tokens.

The adapter applies that declaration to the persisted lines and emits
`licoup.usage-observation.v1` payloads the host pushes through its own
`SourceBinding`. Source epoch, scope and observation ids are the
transport's facts; the store's facts stop at the counter object.

## Invariants

- **Read-only.** It opens session files and never writes the store,
  carries no ledger and opens no connection.
- **Declared, never guessed.** A record the discriminator does not
  match is skipped; a counter field the record does not report stays
  absent — never zero, never `Quality::Reported` on a guess.
- **No per-agent knowledge.** Adding an agent is adding a codec; this
  crate changes only if the canonical spec itself changes.
- **Container stores read through the codec.** Sessions whose
  `SessionRef.key` names a database row go through `decode_session`;
  each canonical `meta` record's `{subtype, data}` is rewrapped as
  `{type, payload}` so the spec's raw-record paths still resolve.
  Undecodable input surfaces as `skipped_lines`, never as "no usage".
- **Temporality is declared.** `TranscriptUsage.temporality` says
  whether counters are per-record deltas or a running series — the
  adapter never guesses which shape a field is.
- **`includes` is set on shared subsets**: cached input inside input,
  input and output inside a reported total — so a total is never summed
  with its own parts.

## What it is not

Not a replacement for `token-usage-detector` (that crate keeps the
collection protocol and the binding/refusal boundary — this adapter
produces payloads for it), and not a parallel store adapter (the codec
remains the only authority on where sessions live and what their
records mean).
