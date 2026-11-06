//! Agent-side usage acquisition read through ACOP transcripts.
//!
//! This crate is the read path the C11 profile expects for agent stores:
//! ACOP's codec owns session discovery and the declared `TranscriptUsage`
//! spec — which root holds counters, which record shape carries them and
//! which fields mean input, cached input, output and total — and this
//! adapter projects that declaration straight onto the persisted lines.
//! No per-agent store knowledge lives here; adding an agent is adding a
//! codec, not touching this crate.
//!
//! The output is `usage` wire payloads ready for a
//! `token-usage-detector` [`token_usage_detector::SourceBinding`]: the
//! caller supplies the epoch and scope because those are the transport's
//! facts, not the store's. Counters a record does not report stay absent —
//! a missing field is never emitted as zero, and a record shape the spec
//! does not match is skipped, never guessed at.
//!
//! The adapter is strictly read-only: it opens session files, never writes
//! to the store, and carries no ledger, no charge and no network surface.

pub mod stored;

use acop::{Codec, SessionEntry, StoreScan, TranscriptUsage, UsageTemporality, store};
use serde_json::{Value, json};
use std::io::BufRead;
use std::path::Path;
use token_usage_detector::metrics;
use token_usage_detector::usage::wire;

/// One metric a `TranscriptUsage` field list maps to, in declaration order.
fn counter_fields(usage: &TranscriptUsage) -> [(&'static str, &'static [&'static str]); 4] {
    [
        (metrics::TOKENS_INPUT, usage.input),
        (metrics::TOKENS_CACHED_INPUT, usage.cached),
        (metrics::TOKENS_OUTPUT, usage.output),
        (metrics::TOKENS_TOTAL, usage.total),
    ]
}

/// Timestamp fields raw transcript lines commonly carry; first present wins.
const RECORD_TIME_FIELDS: &[&str] = &["timestamp", "ts", "time", "created_at"];

/// The wire shape one counter record produces; the temporality is the
/// store's own declaration, never inferred here.
fn counter_metric(value: impl Into<String>, temporality: UsageTemporality) -> Value {
    let temporality = match temporality {
        UsageTemporality::Delta => "delta",
        UsageTemporality::Cumulative => "cumulative",
    };
    json!({
        "value": value.into(),
        "unit": "tokens",
        "temporality": temporality,
        "quality": "reported",
    })
}

/// Resolve a dotted `a.b.c` path inside a JSON value.
fn dotted<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .try_fold(value, |node, segment| node.get(segment))
}

/// Read a numeric counter field as decimal text; non-numeric is absent.
fn counter_value(container: &Value, field: &str) -> Option<String> {
    let value = dotted(container, field)?;
    match value {
        Value::Number(number) => Some(number.to_string()),
        Value::String(text) => text.parse::<i64>().ok().map(|_| text.clone()),
        _ => None,
    }
}

/// A session-backed usage source over one agent codec.
///
/// Built from the codec's declared [`Codec::transcript_usage`]; a codec
/// that declares no transcript counters yields `None` and the agent simply
/// contributes no transcript observations.
pub struct TranscriptSource<'a> {
    codec: &'a dyn Codec,
    usage: TranscriptUsage,
}

/// What one session file yielded: the payloads to publish and the count of
/// undecodable lines, which the caller may surface but the store never
/// explains twice.
pub struct Collected {
    pub payloads: Vec<Value>,
    pub skipped_lines: usize,
    /// The session's bytes ran past [`MAX_SESSION_BYTES`]: `payloads`
    /// holds the explicitly partial prefix the ceiling allowed.
    pub truncated: bool,
}

/// One session's file bytes a single read may walk before it must stop:
/// a runaway or dumped store settles its prefix, never the host's
/// memory. Sized far above a real rollout transcript.
pub const MAX_SESSION_BYTES: u64 = 64 * 1024 * 1024;

/// The read's outcome beyond the payloads themselves.
#[derive(Clone, Copy, Debug, Default)]
pub struct ReadStats {
    /// Native lines that failed to decode — a store defect the caller
    /// must see, since swallowing it would read as "no usage".
    pub skipped_lines: usize,
    /// The session's bytes ran past [`MAX_SESSION_BYTES`]: the read
    /// stopped and the emitted prefix is explicitly partial.
    pub truncated: bool,
}

/// Payloads one emitted batch carries: the publish bound the wire
/// vocabulary already names.
const MAX_BATCH_OBSERVATIONS: usize = token_usage_detector::collection::MAX_BATCH_OBSERVATIONS;

/// The streaming read's open state — decoded container records, a
/// buffered file walk, or the open/read failure surfaced once.
enum BatchState {
    Decoded(std::vec::IntoIter<Value>),
    Lines {
        reader: std::io::BufReader<std::fs::File>,
        bytes: u64,
        line_no: usize,
        delimiter_pending: bool,
        pending: Vec<Value>,
    },
    Failed(Option<acop::Error>),
}

/// What one store line projects to under the spec.
enum LineProjection {
    /// Whitespace or empty — not a record at all.
    Empty,
    /// Undecodable — a store defect the read reports as skipped.
    Malformed,
    /// Decodable but carries none of the declared counters.
    NoCounters,
    /// A settled usage payload.
    Payload(Value),
}

impl<'a> TranscriptSource<'a> {
    /// The usage source a codec declares, when it declares one.
    pub fn of(codec: &'a dyn Codec) -> Option<Self> {
        codec.transcript_usage().map(|usage| Self { codec, usage })
    }

    /// ACOP's session scan narrowed to the root the spec binds counters to.
    pub fn sessions(&self, home: &Path) -> acop::Result<StoreScan> {
        let mut scan = store::scan(self.codec, home)?;
        scan.sessions.retain(|entry| entry.kind == self.usage.root);
        Ok(scan)
    }

    /// Read one session and emit every counter record's payload.
    ///
    /// File-backed sessions are line-read with the spec applied verbatim.
    /// Container-backed sessions (`session.key` set — a database row, not
    /// a file) go through the codec's `decode_session` instead: each
    /// canonical `meta` record's `{subtype, data}` is rewrapped as
    /// `{type, payload}` so the spec's raw-record paths still resolve —
    /// the codec stays the only authority on how a row becomes a record.
    pub fn collect(
        &self,
        entry: &SessionEntry,
        source_epoch: &str,
        scope_ref: &str,
        fallback_observed_at: &str,
    ) -> acop::Result<Collected> {
        let stats = std::cell::Cell::new(ReadStats::default());
        let mut payloads = Vec::new();
        for batch in
            self.payload_batches(entry, source_epoch, scope_ref, fallback_observed_at, &stats)
        {
            payloads.extend(batch?);
        }
        let stats = stats.get();
        Ok(Collected {
            payloads,
            skipped_lines: stats.skipped_lines,
            truncated: stats.truncated,
        })
    }

    /// Stream one session's payloads in publish-sized batches.
    ///
    /// A file-backed session is never materialized: it streams through a
    /// buffered reader bounded to `byte_ceiling` total bytes, every line
    /// is read with the remaining budget (a line the budget cuts mid-way
    /// settles nothing and marks the prefix truncated), and each batch
    /// carries at most [`MAX_BATCH_OBSERVATIONS`] payloads — peak memory
    /// is one batch plus one budgeted line. A container session is
    /// different: `decode_session` materializes the canonical
    /// conversation by contract, so its bound is the codec's own.
    /// `stats` reports skipped lines and truncation as they happen.
    pub fn payload_batches(
        &'a self,
        entry: &'a SessionEntry,
        source_epoch: &'a str,
        scope_ref: &'a str,
        fallback_observed_at: &'a str,
        stats: &'a std::cell::Cell<ReadStats>,
    ) -> impl Iterator<Item = acop::Result<Vec<Value>>> + 'a {
        self.payload_batches_up_to(
            entry,
            source_epoch,
            scope_ref,
            fallback_observed_at,
            stats,
            MAX_SESSION_BYTES,
        )
    }

    /// `payload_batches` with the byte ceiling explicit, so tests can
    /// drive the truncation branch without writing a real 64MiB store.
    fn payload_batches_up_to(
        &'a self,
        entry: &'a SessionEntry,
        source_epoch: &'a str,
        scope_ref: &'a str,
        fallback_observed_at: &'a str,
        stats: &'a std::cell::Cell<ReadStats>,
        byte_ceiling: u64,
    ) -> impl Iterator<Item = acop::Result<Vec<Value>>> + 'a {
        let mut state = if entry.session.key.is_some() {
            match self.collect_decoded(entry, source_epoch, scope_ref, fallback_observed_at) {
                Ok(collected) => {
                    let mut current = stats.get();
                    current.skipped_lines = collected.skipped_lines;
                    stats.set(current);
                    BatchState::Decoded(collected.payloads.into_iter())
                }
                Err(error) => BatchState::Failed(Some(error)),
            }
        } else {
            match std::fs::File::open(&entry.session.path) {
                Ok(file) => BatchState::Lines {
                    reader: std::io::BufReader::new(file),
                    bytes: 0,
                    line_no: 0,
                    delimiter_pending: false,
                    pending: Vec::new(),
                },
                Err(error) => BatchState::Failed(Some(acop::Error::io(&entry.session.path)(error))),
            }
        };
        let session_id = &entry.session.id;
        std::iter::from_fn(move || {
            let flush = |pending: &mut Vec<Value>| match pending.is_empty() {
                true => None,
                false => Some(Ok(std::mem::take(pending))),
            };
            loop {
                match &mut state {
                    BatchState::Failed(error) => return error.take().map(Err),
                    BatchState::Decoded(payloads) => {
                        if payloads.len() == 0 {
                            return None;
                        }
                        return Some(Ok(payloads.by_ref().take(MAX_BATCH_OBSERVATIONS).collect()));
                    }
                    BatchState::Lines {
                        reader,
                        bytes,
                        line_no,
                        delimiter_pending,
                        pending,
                    } => {
                        let remaining = byte_ceiling.saturating_sub(*bytes);
                        if remaining == 0 {
                            // A record projected at the exact boundary
                            // left its `\n` delimiter unconsumed: swallow
                            // it before judging — only real bytes behind
                            // the budget mean the read stopped early.
                            if *delimiter_pending {
                                *delimiter_pending = false;
                                match reader.fill_buf() {
                                    Ok(available) if available.first() == Some(&b'\n') => {
                                        reader.consume(1);
                                    }
                                    Ok(_) => {}
                                    Err(error) => {
                                        return Some(Err(acop::Error::io(&entry.session.path)(
                                            error,
                                        )));
                                    }
                                }
                            }
                            match reader.fill_buf() {
                                Ok(available) => {
                                    if !available.is_empty() {
                                        let mut current = stats.get();
                                        current.truncated = true;
                                        stats.set(current);
                                    }
                                    return flush(pending);
                                }
                                Err(error) => {
                                    return Some(Err(acop::Error::io(&entry.session.path)(error)));
                                }
                            }
                        }
                        let mut raw = Vec::new();
                        let read = match std::io::Read::take(&mut *reader, remaining)
                            .read_until(b'\n', &mut raw)
                        {
                            Ok(read) => read,
                            Err(error) => {
                                return Some(Err(acop::Error::io(&entry.session.path)(error)));
                            }
                        };
                        *bytes += read as u64;
                        if read == 0 {
                            return flush(pending);
                        }
                        if raw.last() != Some(&b'\n') && read as u64 == remaining {
                            // The budget ended exactly on a record: peek
                            // for the byte behind it. A newline means the
                            // record text is complete and only its
                            // delimiter sits outside the budget — project
                            // it and let the next loop's empty budget
                            // mark the prefix. Anything else means the
                            // budget cut mid-record: a partial record
                            // settles nothing.
                            match reader.fill_buf() {
                                Ok(available) if available.first() == Some(&b'\n') => {
                                    *delimiter_pending = true;
                                }
                                Ok(available) if !available.is_empty() => {
                                    let mut current = stats.get();
                                    current.truncated = true;
                                    stats.set(current);
                                    return flush(pending);
                                }
                                Ok(_) => {}
                                Err(error) => {
                                    return Some(Err(acop::Error::io(&entry.session.path)(error)));
                                }
                            }
                        }
                        let ordinal = *line_no;
                        *line_no += 1;
                        let line = String::from_utf8_lossy(&raw);
                        match self.project_line(
                            session_id,
                            ordinal,
                            line.trim(),
                            source_epoch,
                            scope_ref,
                            fallback_observed_at,
                        ) {
                            LineProjection::Payload(payload) => {
                                pending.push(payload);
                                if pending.len() >= MAX_BATCH_OBSERVATIONS {
                                    return Some(Ok(std::mem::take(pending)));
                                }
                            }
                            LineProjection::Malformed => {
                                let mut current = stats.get();
                                current.skipped_lines += 1;
                                stats.set(current);
                            }
                            LineProjection::Empty | LineProjection::NoCounters => {}
                        }
                    }
                }
            }
        })
    }

    /// The spec applied to one raw store line: the projection every read
    /// path shares, so identities and payloads cannot drift between the
    /// streaming walk and `collect_lines`. `line_ordinal` is the line's
    /// own position in the store — every line counts, including empty
    /// and undecodable ones — because settled observation ids carry it:
    /// The stability the id actually promises is bounded: an ordinal
    /// survives its line changing between usage and non-usage content,
    /// but stores are append-only artifacts — any insertion, removal or
    /// reorder of a preceding line renumbers every record behind it and
    /// a rescan settles those records under their new positions.
    fn project_line(
        &self,
        session_id: &str,
        line_ordinal: usize,
        line: &str,
        source_epoch: &str,
        scope_ref: &str,
        fallback_observed_at: &str,
    ) -> LineProjection {
        if line.is_empty() {
            return LineProjection::Empty;
        }
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            return LineProjection::Malformed;
        };
        let Some(metrics) = self.counters(&record) else {
            return LineProjection::NoCounters;
        };
        let observed_at = RECORD_TIME_FIELDS
            .iter()
            .find_map(|field| record.get(field).and_then(Value::as_str))
            .unwrap_or(fallback_observed_at);
        LineProjection::Payload(json!({
            "schema": wire::USAGE,
            "observationId": observation_id(session_id, line_ordinal),
            "revision": 1,
            "operation": "upsert",
            "sourceEpoch": source_epoch,
            "scopeRef": scope_ref,
            "observedAt": observed_at,
            "metrics": metrics,
        }))
    }

    /// The container-store path: canonical records out of `decode_session`,
    /// meta bodies rewrapped into the raw shape the spec's paths name.
    fn collect_decoded(
        &self,
        entry: &SessionEntry,
        source_epoch: &str,
        scope_ref: &str,
        fallback_observed_at: &str,
    ) -> acop::Result<Collected> {
        let decoded = self.codec.decode_session(&entry.session)?;
        let mut payloads = Vec::new();
        for record in &decoded.conversation.records {
            let acop::Body::Meta { subtype, data } = &record.body else {
                continue;
            };
            let raw = json!({"type": subtype, "payload": data});
            let Some(metrics) = self.counters(&raw) else {
                continue;
            };
            let observed_at = record.ts.as_deref().unwrap_or(fallback_observed_at);
            payloads.push(json!({
                "schema": wire::USAGE,
                "observationId": observation_id(&entry.session.id, record.ordinal as usize),
                "revision": 1,
                "operation": "upsert",
                "sourceEpoch": source_epoch,
                "scopeRef": scope_ref,
                "observedAt": observed_at,
                "metrics": metrics,
            }));
        }
        Ok(Collected {
            payloads,
            skipped_lines: decoded.skipped.len(),
            truncated: false,
        })
    }

    /// The line-level projection [`collect`] applies, exposed for callers
    /// holding session text already. `lines` must supply *every* store
    /// line in file order — including empty and undecodable ones — since
    /// each line's own position is the settled observation identity; a
    /// filtered list renumbers records and ids will not replay against
    /// the streaming path or against previously settled facts.
    pub fn collect_lines(
        &self,
        session_id: &str,
        lines: impl Iterator<Item = impl AsRef<str>>,
        source_epoch: &str,
        scope_ref: &str,
        fallback_observed_at: &str,
    ) -> Collected {
        let mut payloads = Vec::new();
        let mut skipped_lines = 0;
        for (index, line) in lines.enumerate() {
            match self.project_line(
                session_id,
                index,
                line.as_ref().trim(),
                source_epoch,
                scope_ref,
                fallback_observed_at,
            ) {
                LineProjection::Payload(payload) => payloads.push(payload),
                LineProjection::Malformed => skipped_lines += 1,
                LineProjection::Empty | LineProjection::NoCounters => {}
            }
        }
        Collected {
            payloads,
            skipped_lines,
            truncated: false,
        }
    }

    /// Apply the spec to one raw record: match the discriminator, read the
    /// counter object, map the declared fields. Absent counters stay absent.
    fn counters(&self, record: &Value) -> Option<Value> {
        if let Some((path, expected)) = self.usage.record
            && dotted(record, path).and_then(Value::as_str) != Some(expected)
        {
            return None;
        }
        let container = dotted(record, self.usage.container)?;
        let mut metrics = serde_json::Map::new();
        for (metric, fields) in counter_fields(&self.usage) {
            if let Some(value) = fields
                .iter()
                .find_map(|field| counter_value(container, field))
            {
                metrics.insert(
                    (*metric).to_owned(),
                    counter_metric(value, self.usage.temporality),
                );
            }
        }
        if metrics.is_empty() {
            return None;
        }
        // `includes` keeps a total from being summed with its own subsets:
        // cached input inside input, input and output inside a reported total.
        if metrics.contains_key(metrics::TOKENS_INPUT)
            && metrics.contains_key(metrics::TOKENS_CACHED_INPUT)
        {
            metrics[metrics::TOKENS_INPUT]["includes"] = json!([metrics::TOKENS_CACHED_INPUT]);
        }
        if metrics.contains_key(metrics::TOKENS_TOTAL) {
            let subsets: Vec<String> = [metrics::TOKENS_INPUT, metrics::TOKENS_OUTPUT]
                .iter()
                .filter(|metric| metrics.contains_key(**metric))
                .map(|metric| (*metric).to_owned())
                .collect();
            metrics[metrics::TOKENS_TOTAL]["includes"] = json!(subsets);
        }
        Some(Value::Object(metrics))
    }
}

/// A bounded, deterministic observation id: the session's own id plus the
/// record's position inside it. The `#L` marker versions the id scheme —
/// any earlier shape (`#<ordinal>` without a scheme marker) is legacy:
/// the collector retires those rows before the first settle so a rescan
/// under this scheme can never collide with them.
fn observation_id(session_id: &str, line_index: usize) -> String {
    const LIMIT: usize = token_usage_detector::usage::MAX_OBSERVATION_ID_BYTES;
    let suffix = format!("#L{line_index}");
    let budget = LIMIT.saturating_sub(suffix.len());
    let end = (0..=budget.min(session_id.len()))
        .rev()
        .find(|&i| session_id.is_char_boundary(i))
        .unwrap_or(0);
    format!("{}{}", &session_id[..end], suffix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use token_usage_detector::usage::UsageObservation;

    const SPEC: TranscriptUsage = TranscriptUsage {
        root: "synthetic-session-store",
        record: Some(("payload.type", "token_count")),
        container: "payload.info.last_token_usage",
        input: &["input_tokens"],
        cached: &["cached_input_tokens"],
        output: &["output_tokens"],
        total: &[],
        temporality: UsageTemporality::Delta,
    };

    fn source() -> TranscriptSource<'static> {
        TranscriptSource {
            codec: &NeverCodec,
            usage: SPEC,
        }
    }

    struct NeverCodec;
    impl Codec for NeverCodec {
        fn id(&self) -> &'static str {
            "synthetic"
        }
        fn history(&self) -> Vec<acop::HistoryRoot> {
            Vec::new()
        }
        fn session_kinds(&self) -> &'static [&'static str] {
            &[]
        }
        fn capabilities(&self) -> &'static acop::CapabilityMatrix {
            &acop::CapabilityMatrix::READ_ONLY
        }
        fn decode(&self, _path: &Path) -> acop::Result<acop::codec::Decoded> {
            unreachable!("tests never decode")
        }
        fn encode(&self, _conversation: &acop::Conversation) -> acop::Result<acop::codec::Encoded> {
            unreachable!("tests never encode")
        }
        fn store_path(
            &self,
            _home: &Path,
            _conversation: &acop::Conversation,
        ) -> acop::Result<std::path::PathBuf> {
            unreachable!("tests never import")
        }
        fn redact_line(&self, _line: &Value, _pattern: &str) -> Option<Value> {
            unreachable!("tests never redact")
        }
    }

    fn lines(lines: &[&str]) -> Collected {
        source().collect_lines(
            "session-1",
            lines.iter(),
            "epoch-1",
            "scope-1",
            "2026-10-08T00:00:00Z",
        )
    }

    #[test]
    fn declared_counters_become_token_metrics() {
        let collected = lines(&[
            r#"{"type":"session_meta","payload":{"id":"s-1"}}"#,
            r#"{"type":"token_usage_record","timestamp":"2026-10-08T12:00:00Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":42,"cached_input_tokens":10,"output_tokens":7}}}}"#,
        ]);
        assert_eq!(collected.payloads.len(), 1);
        let observation =
            UsageObservation::from_value(collected.payloads[0].clone()).expect("wire payload");
        assert_eq!(observation.observed_at, "2026-10-08T12:00:00Z");
        assert_eq!(
            observation.metrics[metrics::TOKENS_INPUT].value.as_deref(),
            Some("42")
        );
        assert_eq!(
            observation.metrics[metrics::TOKENS_INPUT].includes,
            vec![metrics::TOKENS_CACHED_INPUT.to_owned()]
        );
        assert_eq!(
            observation.metrics[metrics::TOKENS_CACHED_INPUT]
                .value
                .as_deref(),
            Some("10")
        );
        assert_eq!(
            observation.metrics[metrics::TOKENS_OUTPUT].value.as_deref(),
            Some("7")
        );
        assert!(!observation.metrics.contains_key(metrics::TOKENS_TOTAL));
    }

    #[test]
    fn absent_counters_stay_absent_and_bad_lines_are_counted() {
        let collected = lines(&[
            r#"{"type":"token_usage_record","payload":{"type":"token_count","info":{"last_token_usage":{}}}}"#,
            "not json",
            r#"{"type":"token_usage_record","payload":{"type":"token_count","info":{"last_token_usage":{"output_tokens":5}}}}"#,
        ]);
        assert_eq!(collected.skipped_lines, 1);
        assert_eq!(collected.payloads.len(), 1);
        let observation =
            UsageObservation::from_value(collected.payloads[0].clone()).expect("wire payload");
        assert!(!observation.metrics.contains_key(metrics::TOKENS_INPUT));
        assert_eq!(
            observation.metrics[metrics::TOKENS_OUTPUT].value.as_deref(),
            Some("5")
        );
    }

    #[test]
    fn the_discriminator_rejects_other_records() {
        let collected = lines(&[
            r#"{"type":"event_msg","payload":{"type":"turn_diff","info":{"last_token_usage":{"input_tokens":9}}}}"#,
            r#"{"payload":{"type":"other","info":{"last_token_usage":{"input_tokens":9}}}}"#,
        ]);
        assert!(collected.payloads.is_empty());
    }

    // The codex rollout keeps its canonical usage envelope in a top-level
    // `token_usage_record` record and mirrors the same `token_count` payload
    // into the `event_msg` stream for the UI. The declared discriminator is
    // the outer `type`; matching `payload.type` instead would settle every
    // turn twice. This fixture mirrors `a4aa-codex`'s `TranscriptUsage`.
    const CODEX_SPEC: TranscriptUsage = TranscriptUsage {
        root: "codex-sessions",
        record: Some(("type", "token_usage_record")),
        container: "payload.info.last_token_usage",
        input: &["input_tokens"],
        cached: &["cached_input_tokens"],
        output: &["output_tokens"],
        total: &[],
        temporality: UsageTemporality::Delta,
    };

    #[test]
    fn codex_rollout_shape_matches_and_event_msg_mirror_is_skipped() {
        let collected = TranscriptSource {
            codec: &NeverCodec,
            usage: CODEX_SPEC,
        }
        .collect_lines(
            "rollout-1",
            [
                r#"{"type":"session_meta","payload":{"id":"r-1"}}"#,
                r#"{"type":"token_usage_record","timestamp":"2026-10-24T09:30:00Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":900,"cached_input_tokens":100,"output_tokens":40,"total_tokens":940},"last_token_usage":{"input_tokens":300,"cached_input_tokens":50,"output_tokens":15,"reasoning_output_tokens":4,"total_tokens":319}},"rate_limits":{"primary":{"used_percent":3}}}}"#,
                r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":900,"cached_input_tokens":100,"output_tokens":40,"total_tokens":940},"last_token_usage":{"input_tokens":300,"cached_input_tokens":50,"output_tokens":15,"reasoning_output_tokens":4,"total_tokens":319}},"rate_limits":{"primary":{"used_percent":3}}}}"#,
            ]
            .iter(),
            "epoch-1",
            "scope-1",
            "2026-10-24T09:30:01Z",
        );
        assert_eq!(collected.payloads.len(), 1);
        let observation =
            UsageObservation::from_value(collected.payloads[0].clone()).expect("wire payload");
        assert_eq!(observation.observed_at, "2026-10-24T09:30:00Z");
        assert_eq!(
            observation.metrics[metrics::TOKENS_INPUT].value.as_deref(),
            Some("300")
        );
        assert_eq!(
            observation.metrics[metrics::TOKENS_CACHED_INPUT]
                .value
                .as_deref(),
            Some("50")
        );
        assert_eq!(
            observation.metrics[metrics::TOKENS_OUTPUT].value.as_deref(),
            Some("15")
        );
    }

    // Container-backed stores (a database row per session, `SessionRef.key`
    // set) are read through the codec's `decode_session`; each Meta record
    // is rewrapped `{subtype, data}` → `{type, payload}` so the spec's raw
    // paths still resolve. The decoder's skipped lines stay counted.
    struct ContainerCodec;
    impl Codec for ContainerCodec {
        fn id(&self) -> &'static str {
            "synthetic-container"
        }
        fn history(&self) -> Vec<acop::HistoryRoot> {
            Vec::new()
        }
        fn session_kinds(&self) -> &'static [&'static str] {
            &[]
        }
        fn capabilities(&self) -> &'static acop::CapabilityMatrix {
            &acop::CapabilityMatrix::READ_ONLY
        }
        fn decode(&self, _path: &Path) -> acop::Result<acop::codec::Decoded> {
            unreachable!("container sessions decode by key")
        }
        fn decode_session(
            &self,
            _session: &acop::codec::SessionRef,
        ) -> acop::Result<acop::codec::Decoded> {
            let mut conversation = acop::Conversation::new("row-1");
            conversation.records = vec![
                acop::Record {
                    id: None,
                    parent: None,
                    ts: Some("2026-10-24T09:30:00Z".to_owned()),
                    ordinal: 0,
                    body: acop::Body::Meta {
                        subtype: "session_meta".to_owned(),
                        data: json!({"id": "row-1"}),
                    },
                },
                acop::Record {
                    id: None,
                    parent: None,
                    ts: Some("2026-10-24T09:31:00Z".to_owned()),
                    ordinal: 1,
                    body: acop::Body::Meta {
                        subtype: "token_usage_record".to_owned(),
                        data: json!({
                            "type": "token_count",
                            "info": {
                                "last_token_usage": {
                                    "input_tokens": 20,
                                    "cached_input_tokens": 4,
                                    "output_tokens": 3
                                }
                            }
                        }),
                    },
                },
            ];
            Ok(acop::codec::Decoded {
                conversation,
                record_lines: vec![0, 1],
                line_count: 2,
                skipped: vec![acop::codec::Skipped {
                    line: 2,
                    native_type: None,
                    reason: "corrupt".to_owned(),
                }],
            })
        }
        fn encode(&self, _conversation: &acop::Conversation) -> acop::Result<acop::codec::Encoded> {
            unreachable!("read-only")
        }
        fn store_path(
            &self,
            _home: &Path,
            _conversation: &acop::Conversation,
        ) -> acop::Result<std::path::PathBuf> {
            unreachable!("read-only")
        }
        fn redact_line(&self, _line: &Value, _pattern: &str) -> Option<Value> {
            unreachable!("read-only")
        }
    }

    #[test]
    fn container_sessions_decode_by_key_and_count_decoder_skips() {
        let entry = acop::SessionEntry {
            session: acop::codec::SessionRef {
                id: "row-1".to_owned(),
                path: std::path::PathBuf::from("store.db"),
                key: Some("composerData:1".to_owned()),
                parent: None,
            },
            kind: "codex-sessions",
        };
        let collected = TranscriptSource {
            codec: &ContainerCodec,
            usage: CODEX_SPEC,
        }
        .collect(&entry, "epoch-1", "scope-1", "2026-10-24T09:31:01Z")
        .expect("container collect");
        assert_eq!(collected.skipped_lines, 1);
        assert_eq!(collected.payloads.len(), 1);
        let observation =
            UsageObservation::from_value(collected.payloads[0].clone()).expect("wire payload");
        assert_eq!(observation.observed_at, "2026-10-24T09:31:00Z");
        assert_eq!(
            observation.metrics[metrics::TOKENS_INPUT].value.as_deref(),
            Some("20")
        );
        assert_eq!(
            observation.metrics[metrics::TOKENS_OUTPUT].value.as_deref(),
            Some("3")
        );
    }

    #[test]
    fn observation_ids_truncate_on_byte_boundaries() {
        const LIMIT: usize = token_usage_detector::usage::MAX_OBSERVATION_ID_BYTES;
        let session = "会话-".repeat(LIMIT); // multibyte, far over budget
        let id = observation_id(&session, 12);
        assert!(id.len() <= LIMIT, "{} > {LIMIT}", id.len());
        assert!(id.ends_with("#L12"));
        // Deterministic and never splits a codepoint.
        assert_eq!(id, observation_id(&session, 12));
        std::str::from_utf8(id.as_bytes()).expect("valid utf-8 by construction");
    }

    #[test]
    fn reads_stop_at_the_byte_ceiling_and_mark_the_prefix_partial() {
        let dir = std::env::temp_dir()
            .join(format!("acop-usage-src-{}", std::process::id()))
            .join("ceiling");
        std::fs::create_dir_all(&dir).unwrap();
        let line = "{\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":2,\"output_tokens\":1}}}}\n";
        let path = dir.join("session.jsonl");
        std::fs::write(&path, line.repeat(20)).unwrap();
        let entry = SessionEntry {
            session: acop::codec::SessionRef {
                id: "session-ceiling".to_owned(),
                path,
                key: None,
                parent: None,
            },
            kind: "synthetic-session-store",
        };
        let source = source();
        let stats = std::cell::Cell::new(ReadStats::default());
        let payloads: Vec<Value> = source
            .payload_batches_up_to(
                &entry,
                "epoch",
                "scope",
                "2026-11-02T00:00:00Z",
                &stats,
                line.len() as u64 * 3,
            )
            .collect::<acop::Result<Vec<_>>>()
            .unwrap()
            .into_iter()
            .flatten()
            .collect();
        let stats = stats.get();
        assert!(stats.truncated);
        assert!(!payloads.is_empty() && payloads.len() < 20);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn write_session(
        lines: &[&str],
        name: &str,
        kind: &'static str,
    ) -> (std::path::PathBuf, SessionEntry) {
        let dir = std::env::temp_dir()
            .join(format!("acop-usage-src-{}", std::process::id()))
            .join(name.trim_end_matches(".jsonl"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, lines.concat()).unwrap();
        (
            dir,
            SessionEntry {
                session: acop::codec::SessionRef {
                    id: "parity".to_owned(),
                    path,
                    key: None,
                    parent: None,
                },
                kind,
            },
        )
    }

    #[test]
    fn streamed_ids_match_collect_lines_regardless_of_record_shape() {
        // Meta and mirror lines interleave with usage records; the settled
        // identity is the line's own position — the same ids
        // `collect_lines` names them.
        let session_lines = [
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"p\"}}\n",
            "{\"type\":\"token_usage_record\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":1}}}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{}}}}\n",
            "{\"type\":\"token_usage_record\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":2}}}}\n",
        ];
        let (dir, entry) = write_session(&session_lines, "parity.jsonl", "codex-sessions");
        let codex = TranscriptSource {
            codec: &NeverCodec,
            usage: CODEX_SPEC,
        };
        let collected = codex.collect_lines(
            "parity",
            session_lines.iter().map(|line| line.trim_end()),
            "epoch",
            "scope",
            "2026-11-02T00:00:00Z",
        );
        let expected: Vec<String> = collected
            .payloads
            .iter()
            .map(|payload| payload["observationId"].as_str().unwrap().to_owned())
            .collect();
        let stats = std::cell::Cell::new(ReadStats::default());
        let streamed: Vec<String> = codex
            .payload_batches_up_to(
                &entry,
                "epoch",
                "scope",
                "2026-11-02T00:00:00Z",
                &stats,
                u64::MAX,
            )
            .collect::<acop::Result<Vec<_>>>()
            .unwrap()
            .into_iter()
            .flatten()
            .map(|payload| payload["observationId"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(streamed, expected);
        assert_eq!(streamed, vec!["parity#L1", "parity#L3"]);
        assert!(!stats.get().truncated);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_record_filling_the_budget_with_newline_behind_still_settles() {
        // The JSON text ends exactly at the ceiling and only its `\n`
        // delimiter sits outside it — the record is complete and projects.
        let usage = "{\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":2}}}}\n";
        let (dir, entry) = write_session(
            &[usage, usage, usage],
            "exact.jsonl",
            "synthetic-session-store",
        );
        let stats = std::cell::Cell::new(ReadStats::default());
        let payloads: Vec<Value> = source()
            .payload_batches_up_to(
                &entry,
                "epoch",
                "scope",
                "2026-11-02T00:00:00Z",
                &stats,
                usage.len() as u64 - 1, // budget ends on the record's last byte
            )
            .collect::<acop::Result<Vec<_>>>()
            .unwrap()
            .into_iter()
            .flatten()
            .collect();
        assert_eq!(payloads.len(), 1);
        assert!(stats.get().truncated, "further bytes sit behind the line");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_ending_exactly_at_the_budget_is_not_truncated() {
        let usage = "{\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":3}}}}\n";
        let two = [usage, usage];
        let (dir, entry) = write_session(&two, "ends.jsonl", "synthetic-session-store");
        let stats = std::cell::Cell::new(ReadStats::default());
        let payloads: Vec<Value> = source()
            .payload_batches_up_to(
                &entry,
                "epoch",
                "scope",
                "2026-11-02T00:00:00Z",
                &stats,
                usage.len() as u64 * 2,
            )
            .collect::<acop::Result<Vec<_>>>()
            .unwrap()
            .into_iter()
            .flatten()
            .collect();
        assert_eq!(payloads.len(), 2);
        assert!(!stats.get().truncated);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_single_record_ending_on_the_budget_is_complete_not_truncated() {
        // The file's one record fills the budget and only its `\n`
        // delimiter sits behind it: every record settles and the prefix
        // is not partial.
        let usage = "{\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":4}}}}\n";
        let (dir, entry) = write_session(&[usage], "one.jsonl", "synthetic-session-store");
        let stats = std::cell::Cell::new(ReadStats::default());
        let payloads: Vec<Value> = source()
            .payload_batches_up_to(
                &entry,
                "epoch",
                "scope",
                "2026-11-02T00:00:00Z",
                &stats,
                usage.len() as u64 - 1,
            )
            .collect::<acop::Result<Vec<_>>>()
            .unwrap()
            .into_iter()
            .flatten()
            .collect();
        assert_eq!(payloads.len(), 1);
        assert!(
            !stats.get().truncated,
            "only the trailing delimiter was unread"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
