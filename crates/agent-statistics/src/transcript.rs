//! Agent-side transcript collection wired through the standard source
//! entry: `acop-usage-source` reads declared counters out of ACOP
//! transcripts, and this module pushes the resulting payloads through
//! `usage.publish` exactly like any other source — bound by the host's
//! admission, never self-asserted.
//!
//! The binding minted here names the codec as the source: one binding per
//! agent codec, one epoch per collector run, scopes the host grants.

use acativa::usage_ledger::LedgerError;
use acop::SessionEntry;
use acop_usage_source::{ReadStats, TranscriptSource};
use anyhow::{Context, Result};
use token_usage_detector::collection::PublishBatch;
use token_usage_detector::{RecoveryAction, SourceBinding};

use crate::journal::{IngestReport, SourceAdmission};
use crate::source_host::UsageSourceHost;

/// The extension id transcript collection settles under — one logical
/// producer no matter which codec read the store.
pub const TRANSCRIPT_EXTENSION_ID: &str = "acop.transcript";

/// The scope transcript observations settle under: the aggregate range a
/// host grants when it collects agent-side counters.
pub const TRANSCRIPT_SCOPE: &str = "agent:transcripts";

/// The binding one codec's transcript collection presents — the source
/// identity names the codec so facts stay attributable to the store they
/// came from; epoch and scopes are the host's facts, supplied by the
/// caller the way the transport would.
pub fn transcript_binding(
    codec_id: &str,
    instance_id: &str,
    source_epoch: &str,
    scope_grants: &[&str],
) -> Result<SourceBinding> {
    SourceBinding::new(
        format!("source:acop-transcript#{codec_id}"),
        TRANSCRIPT_EXTENSION_ID,
        instance_id,
        1,
        source_epoch,
        scope_grants.iter().copied(),
    )
    .context("transcript source binding")
}

/// The settled ingest plus the store defects the read observed.
#[derive(Debug)]
pub struct TranscriptReport {
    /// The journal-side result: accepted, refused, events and cursor.
    pub ingest: IngestReport,
    /// Store lines the read could not decode — a store defect the caller
    /// must see, since swallowing it would read as "no usage".
    pub skipped_lines: usize,
    /// The session ran past the read's byte ceiling: the settled prefix
    /// is explicitly partial, never silently complete.
    pub truncated: bool,
}

/// Collect one session's declared counters into the journal.
///
/// The epoch and scope inside each payload are the admission's own: the
/// payload carries what the host bound, never a second opinion about who
/// produced it. The read streams publish-sized batches and each batch
/// commits in its own bounded transaction (`publish_all`) — a long
/// session never pins the WAL for its whole read. `skipped_lines` and
/// `truncated` stay visible to the caller: undecodable store lines are
/// the store's defect, and a session past the byte ceiling settles its
/// prefix, never a silent "no usage".
pub fn collect_transcript_session(
    host: &mut UsageSourceHost,
    admission: &SourceAdmission,
    source: &TranscriptSource<'_>,
    entry: &SessionEntry,
    scope_ref: &str,
    fallback_observed_at: &str,
) -> Result<TranscriptReport> {
    let stats = std::cell::Cell::new(ReadStats::default());
    // The codec's own error (path and cause) rides the report's context
    // through this cell — the ledger surface itself carries only the
    // refusal vocabulary, never a filesystem path.
    let read_cause = std::cell::RefCell::new(None::<acop::Error>);
    let batches = source
        .payload_batches(
            entry,
            &admission.binding().source_epoch,
            scope_ref,
            fallback_observed_at,
            &stats,
        )
        .map(|batch| {
            batch
                .map_err(|error| {
                    // A missing or unreadable store is a permanent input
                    // defect; a mid-read io failure is transient like any
                    // storage call.
                    let retryable = match &error {
                        acop::Error::Io { source, .. } => !matches!(
                            source.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                        ),
                        _ => false,
                    };
                    let action = match retryable {
                        true => RecoveryAction::RetryAfterRecovery,
                        false => RecoveryAction::CorrectRequest,
                    };
                    *read_cause.borrow_mut() = Some(error);
                    LedgerError {
                        code: "transcript_read_failed".into(),
                        stage: "acop.transcript".into(),
                        retryable,
                        recovery: action.wire().to_owned(),
                    }
                })
                .and_then(|observations| {
                    PublishBatch::new(observations).map_err(|_| {
                        LedgerError::refused("acop.transcript", "transcript_batch_invalid")
                    })
                })
        });
    let ingest = match host.publish_all(admission, batches) {
        Ok(report) => report,
        Err(error) => match read_cause.borrow_mut().take() {
            Some(cause) => return Err(anyhow::Error::new(error).context(format!("{cause}"))),
            None => {
                return Err(error)
                    .with_context(|| format!("transcript session `{}`", entry.session.id));
            }
        },
    };
    let stats = stats.get();
    Ok(TranscriptReport {
        ingest,
        skipped_lines: stats.skipped_lines,
        truncated: stats.truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use acop::codec::SessionRef;
    use acop::{CapabilityMatrix, Codec, HistoryRoot, TranscriptUsage, UsageTemporality};
    use serde_json::{Value, json};
    use std::path::{Path, PathBuf};
    use token_usage_detector::collection::MAX_BATCH_OBSERVATIONS;

    const SPEC: TranscriptUsage = TranscriptUsage {
        root: "synthetic-session-store",
        record: Some(("type", "usage_record")),
        container: "payload.usage",
        input: &["input_tokens"],
        cached: &[],
        output: &["output_tokens"],
        total: &[],
        temporality: UsageTemporality::Delta,
    };

    struct StubCodec;
    impl Codec for StubCodec {
        fn id(&self) -> &'static str {
            "synthetic"
        }
        fn history(&self) -> Vec<HistoryRoot> {
            Vec::new()
        }
        fn session_kinds(&self) -> &'static [&'static str] {
            &["synthetic-session-store"]
        }
        fn capabilities(&self) -> &'static CapabilityMatrix {
            &CapabilityMatrix::READ_ONLY
        }
        fn transcript_usage(&self) -> Option<TranscriptUsage> {
            Some(SPEC)
        }
        fn decode(&self, _path: &Path) -> acop::Result<acop::codec::Decoded> {
            unreachable!("file-backed sessions are line-read")
        }
        fn encode(&self, _conversation: &acop::Conversation) -> acop::Result<acop::codec::Encoded> {
            unreachable!("read-only")
        }
        fn store_path(
            &self,
            _home: &Path,
            _conversation: &acop::Conversation,
        ) -> acop::Result<PathBuf> {
            unreachable!("read-only")
        }
        fn redact_line(&self, _line: &Value, _pattern: &str) -> Option<Value> {
            unreachable!("read-only")
        }
    }

    #[test]
    fn transcript_counters_settle_into_the_journal() {
        let dir = std::env::temp_dir().join(format!("metera-transcript-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let session_path = dir.join("session-1.jsonl");
        std::fs::write(
            &session_path,
            "{\"type\":\"usage_record\",\"payload\":{\"usage\":{\"input_tokens\":11,\"output_tokens\":4}}}\n",
        )
        .unwrap();
        let entry = SessionEntry {
            session: SessionRef {
                id: "session-1".to_owned(),
                path: session_path,
                key: None,
                parent: None,
            },
            kind: "synthetic-session-store",
        };
        let mut host =
            UsageSourceHost::open(&json!({"stateRoot": dir.to_string_lossy()})).expect("host");
        let binding = transcript_binding("synthetic", "pid-test", "epoch-1", &[TRANSCRIPT_SCOPE])
            .expect("binding");
        let admission = host.admit_source(
            binding,
            crate::journal::SettlementEligibility::SettlementEligible,
        );
        let source = TranscriptSource::of(&StubCodec).expect("spec declared");
        let report = collect_transcript_session(
            &mut host,
            &admission,
            &source,
            &entry,
            TRANSCRIPT_SCOPE,
            "2026-11-02T00:00:00Z",
        )
        .expect("collect");
        assert_eq!(report.ingest.accepted, 1);
        assert_eq!(report.skipped_lines, 0);
        assert!(!report.truncated);
        assert!(report.ingest.refused.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_session_longer_than_one_batch_settles_in_one_publish() {
        let dir = std::env::temp_dir().join(format!("metera-transcript-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let records = MAX_BATCH_OBSERVATIONS + 3;
        let body: String = (0..records)
            .map(|index| {
                format!(
                    "{{\"type\":\"usage_record\",\"payload\":{{\"usage\":{{\"input_tokens\":{index},\"output_tokens\":1}}}}}}\n"
                )
            })
            .collect();
        let session_path = dir.join("session-long.jsonl");
        std::fs::write(&session_path, body).unwrap();
        let entry = SessionEntry {
            session: SessionRef {
                id: "session-long".to_owned(),
                path: session_path,
                key: None,
                parent: None,
            },
            kind: "synthetic-session-store",
        };
        let mut host =
            UsageSourceHost::open(&json!({"stateRoot": dir.to_string_lossy()})).expect("host");
        let binding = transcript_binding("synthetic", "pid-test", "epoch-1", &[TRANSCRIPT_SCOPE])
            .expect("binding");
        let admission = host.admit_source(
            binding,
            crate::journal::SettlementEligibility::SettlementEligible,
        );
        let source = TranscriptSource::of(&StubCodec).expect("spec declared");
        let report = collect_transcript_session(
            &mut host,
            &admission,
            &source,
            &entry,
            TRANSCRIPT_SCOPE,
            "2026-11-02T00:00:00Z",
        )
        .expect("collect");
        assert_eq!(report.ingest.accepted, records);
        assert!(report.ingest.refused.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
