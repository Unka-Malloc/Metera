//! Bounded JSON collection storage for local client state.
//!
//! A [`CollectionStore`] owns one private directory. Each named collection is
//! one JSON document written atomically: `schemaVersion` and `collection` are
//! stamped on the way in and checked on the way out, and the encoded document
//! is capped at [`MAX_COLLECTION_DOCUMENT_BYTES`]. Overflow is refused rather
//! than truncated.
//!
//! [`CollectionStore::write_collection_retaining_latest_items`] keeps a
//! complete newest suffix of an oldest-first `items` array. A writer that
//! appends usage reports can therefore bound both count and bytes while never
//! publishing a half-record.
//!
//! This crate is the single implementation `agent-statistics` stores
//! through: retained reports persist as bounded collection documents, and
//! the gateway request counters write through the private-file rules in
//! [`fs`] — owner-only directories, atomic writes, bounded reads.

pub mod fs;

use anyhow::{Context, Result, anyhow, ensure};
use serde_json::{Value, json};
use std::env;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// The collection document schema this store reads and writes.
pub const STATE_SCHEMA_VERSION: &str = "v0.0.1:schema:definition-1";

/// The directory beneath a data home that holds collection documents.
pub const CLIENT_STATE_DIR: &str = "client-state";

/// The largest encoded collection document this store publishes. This is a
/// compiled security invariant, not a deployment tuning knob: a client-state
/// document that outgrew the bound would be a defect in the producer, and a
/// configurable ceiling could silently be raised past the privacy and
/// memory guarantees the bound exists to provide.
pub const MAX_COLLECTION_DOCUMENT_BYTES: usize = 16 * 1024 * 1024;

/// One private directory of bounded JSON collections.
#[derive(Clone, Debug)]
pub struct CollectionStore {
    root: PathBuf,
}

impl CollectionStore {
    /// Open a store under the composed portable data home, creating the
    /// directory when it does not exist.
    pub fn portable() -> Result<Self> {
        Self::new(portable_state_root()?)
    }

    /// Open a store under `root`, creating it as an owner-only directory.
    pub fn new(root: PathBuf) -> Result<Self> {
        fs::ensure_private_dir(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The file one collection is persisted to.
    pub fn collection_path(&self, collection: &str) -> Result<PathBuf> {
        validate_collection(collection)?;
        Ok(self.root.join(format!("{collection}.json")))
    }

    /// Read one collection. A missing file reads as the empty document for
    /// this collection; an empty, oversized, malformed or foreign-owned file
    /// is an error.
    pub fn read_collection(&self, collection: &str) -> Result<Value> {
        let path = self.collection_path(collection)?;
        reject_empty_existing_collection(&path)?;
        let document = read_json_or_default(&path, MAX_COLLECTION_DOCUMENT_BYTES, || {
            empty_collection(collection)
        })?;
        validate_current_collection_document(collection, &document)?;
        Ok(document)
    }

    /// Write one collection atomically. The returned value is the normalized
    /// document that was persisted.
    pub fn write_collection(&self, collection: &str, value: Value) -> Result<Value> {
        let document = normalize_collection(collection, value)?;
        write_normalized_collection(&self.collection_path(collection)?, &document)?;
        Ok(document)
    }

    /// Persist complete newest items from an oldest-first sequence, bounded
    /// both by count and by the encoded collection size.
    pub fn write_collection_retaining_latest_items(
        &self,
        collection: &str,
        value: Value,
        max_items: usize,
    ) -> Result<Value> {
        let mut document = normalize_collection(collection, value)?;
        retain_latest_items(&mut document, max_items, MAX_COLLECTION_DOCUMENT_BYTES)?;
        write_normalized_collection(&self.collection_path(collection)?, &document)?;
        Ok(document)
    }
}

/// Project the owned layout without opening or initializing a data home.
pub fn state_root_for_data_home(root: &Path) -> PathBuf {
    root.join(CLIENT_STATE_DIR)
}

/// Resolve the portable data home used when a caller names no state root.
///
/// The environment wins, then the boot locator shared by the desktop and its
/// sidecars, then the default `~/.lico-up`. A saved root that is currently
/// unavailable is an error rather than a new empty directory: silently moving
/// a user's retained reports would make earlier scans disappear.
pub fn portable_data_dir() -> Result<PathBuf> {
    if let Some(path) = environment_data_home()? {
        return Ok(path);
    }
    let saved = read_saved_data_home()?;
    let home = user_home_from_env()
        .map(|home| strip_macos_data_volume(&home))
        .ok_or_else(|| anyhow!("cannot resolve the LicoUp home directory"))?;
    match saved {
        Some(path) => {
            if !path.is_dir() {
                return Err(anyhow!("saved LicoUp data root is unavailable"));
            }
            Ok(path)
        }
        None => Ok(home.join(".lico-up")),
    }
}

fn environment_data_home() -> Result<Option<PathBuf>> {
    resolve_env_data_home(
        |name| env::var_os(name),
        || env::current_dir().context("current directory could not be resolved"),
    )
}

/// Environment resolution, split from the `env` lookup so precedence,
/// placeholder handling and relative-root joining are testable without
/// mutating process state.
fn resolve_env_data_home(
    lookup: impl Fn(&str) -> Option<std::ffi::OsString>,
    current_dir: impl FnOnce() -> Result<PathBuf>,
) -> Result<Option<PathBuf>> {
    for name in ["LICOUP_HOME", "LICOUP_PORTABLE_DIR"] {
        let Some(value) = lookup(name) else {
            continue;
        };
        let text = value.to_string_lossy();
        let trimmed = text.trim();
        if trimmed.is_empty() || trimmed.starts_with('$') || trimmed.contains("${") {
            continue;
        }
        let path = PathBuf::from(trimmed);
        return Ok(Some(if path.is_absolute() {
            normalize_path(&path)
        } else {
            normalize_path(&current_dir()?.join(path))
        }));
    }
    Ok(None)
}

fn data_home_locator_path() -> Result<PathBuf> {
    let home = user_home_from_env()
        .map(|home| strip_macos_data_volume(&home))
        .ok_or_else(|| anyhow!("cannot resolve the LicoUp home directory"))?;
    let config = if cfg!(target_os = "macos") {
        home.join("Library/Application Support/LicoUp")
    } else if cfg!(target_os = "windows") {
        env::var_os("APPDATA")
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| home.join("AppData/Roaming"))
            .join("LicoUp")
    } else {
        env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| home.join(".config"))
            .join("licoup")
    };
    Ok(config.join("data-home"))
}

fn read_saved_data_home() -> Result<Option<PathBuf>> {
    let locator = data_home_locator_path()?;
    let Some(value) = fs::read_private_text_bounded(&locator, 32 * 1024)? else {
        return Ok(None);
    };
    parse_saved_data_home(&value).map(Some)
}

/// The boot locator's document is one absolute path; anything else —
/// blank, relative, a placeholder — is invalid rather than a fallback,
/// because silently relocating retained reports would strand them.
fn parse_saved_data_home(value: &str) -> Result<PathBuf> {
    let value = value.trim();
    if value.is_empty() {
        return Err(anyhow!("saved LicoUp data root is invalid"));
    }
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(anyhow!("saved LicoUp data root is invalid"));
    }
    Ok(normalize_path(&path))
}

fn user_home_from_env() -> Option<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .or_else(|| {
            let drive = env::var_os("HOMEDRIVE")?;
            let path = env::var_os("HOMEPATH")?;
            let mut joined = PathBuf::from(drive);
            joined.push(path);
            Some(joined.into_os_string())
        })
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

fn strip_macos_data_volume(path: &Path) -> PathBuf {
    let prefix = Path::new("/System/Volumes/Data");
    match path.strip_prefix(prefix) {
        Ok(rest) if rest.as_os_str().is_empty() => PathBuf::from("/"),
        Ok(rest) => Path::new("/").join(rest),
        Err(_) => path.to_path_buf(),
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(
                    normalized.components().next_back(),
                    Some(Component::Normal(_))
                ) {
                    normalized.pop();
                } else if !normalized.has_root() {
                    normalized.push(component.as_os_str());
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        normalized
    }
}

/// The portable collection root: `<data home>/client-state`.
pub fn portable_state_root() -> Result<PathBuf> {
    Ok(state_root_for_data_home(&portable_data_dir()?))
}

fn validate_collection(collection: &str) -> Result<()> {
    ensure!(
        !collection.is_empty()
            && collection.len() <= 128
            && collection
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "unsupported client state collection"
    );
    Ok(())
}

fn empty_collection(collection: &str) -> Value {
    json!({
        "schemaVersion": STATE_SCHEMA_VERSION,
        "collection": collection,
        "items": []
    })
}

fn normalize_collection(collection: &str, value: Value) -> Result<Value> {
    if let Value::Object(mut object) = value {
        if let Some(schema) = object.get("schemaVersion") {
            ensure!(
                schema.as_str() == Some(STATE_SCHEMA_VERSION),
                "client state collection requires startup migration"
            );
        }
        if let Some(owner) = object.get("collection") {
            ensure!(
                owner.as_str() == Some(collection),
                "client state collection owner mismatch"
            );
        }
        object
            .entry("schemaVersion".to_owned())
            .or_insert_with(|| json!(STATE_SCHEMA_VERSION));
        object
            .entry("collection".to_owned())
            .or_insert_with(|| json!(collection));
        Ok(Value::Object(object))
    } else {
        Ok(json!({
            "schemaVersion": STATE_SCHEMA_VERSION,
            "collection": collection,
            "items": value
        }))
    }
}

fn validate_current_collection_document(collection: &str, document: &Value) -> Result<()> {
    ensure!(
        document.is_object(),
        "client state collection is not an object"
    );
    ensure!(
        document.get("schemaVersion").and_then(Value::as_str) == Some(STATE_SCHEMA_VERSION),
        "client state collection requires startup migration"
    );
    ensure!(
        document.get("collection").and_then(Value::as_str) == Some(collection),
        "client state collection owner mismatch"
    );
    Ok(())
}

fn read_json_or_default<F>(path: &Path, max_bytes: usize, default_value: F) -> Result<Value>
where
    F: FnOnce() -> Value,
{
    let Some(raw) = fs::read_private_text_bounded(path, max_bytes)? else {
        return Ok(default_value());
    };
    // Only a missing file defaults: a file that exists but carries no
    // document is truncated or corrupt, and silently reporting it as empty
    // would hide a state the caller should refuse.
    ensure!(
        !raw.trim().is_empty(),
        "client state collection is malformed"
    );
    Ok(serde_json::from_str(&raw)?)
}

fn write_normalized_collection(path: &Path, document: &Value) -> Result<()> {
    fs::atomic_write_json(path, document)
}

fn reject_empty_existing_collection(path: &Path) -> Result<()> {
    if let Some(len) = fs::leaf_regular_len(path)? {
        ensure!(len > 0, "client state collection is empty");
    }
    Ok(())
}

/// Keep a complete newest suffix of an oldest-first collection. Each entry is
/// measured at the same indentation depth as the actual document, so strings,
/// Unicode, escaping and nested objects use the writer's own codec.
fn retain_latest_items(document: &mut Value, max_items: usize, max_bytes: usize) -> Result<()> {
    ensure!(
        max_items > 0,
        "collection retention requires at least one item"
    );
    let mut items = document
        .get_mut("items")
        .and_then(Value::as_array_mut)
        .map(std::mem::take)
        .ok_or_else(|| anyhow!("collection retention requires an items array"))?;
    let empty_bytes = encoded_json_bytes(document)?;
    if items.is_empty() {
        if empty_bytes > max_bytes {
            document["items"] = Value::Array(items);
            return Err(anyhow!("collection metadata exceeds its bounded size"));
        }
        document["items"] = Value::Array(items);
        return Ok(());
    }
    #[derive(serde::Serialize)]
    struct Items<'a> {
        items: &'a [&'a Value],
    }
    let empty_wrapper = encoded_json_bytes(&Items { items: &[] })?;
    // A populated pretty array adds its opening/closing line breaks. Each
    // additional entry has the same byte contribution after its separator.
    let mut bytes = empty_bytes.saturating_add(2);
    let mut retained = 0;
    for item in items.iter().rev().take(max_items) {
        let item_bytes =
            encoded_json_bytes(&Items { items: &[item] })?.saturating_sub(empty_wrapper + 2);
        if bytes.saturating_add(item_bytes) > max_bytes {
            break;
        }
        bytes += item_bytes;
        retained += 1;
    }
    if retained == 0 {
        document["items"] = Value::Array(items);
        return Err(anyhow!(
            "complete latest collection item exceeds its bounded size"
        ));
    }
    items.drain(..items.len() - retained);
    document["items"] = Value::Array(items);
    // The incremental account is exact for the current codec; verify the
    // candidate against the writer's real encoding once so a codec change
    // can never let the written document exceed the bound.
    while encoded_json_bytes(document)? > max_bytes {
        let items = document["items"]
            .as_array_mut()
            .expect("items array restored");
        if items.len() <= 1 {
            return Err(anyhow!(
                "complete latest collection item exceeds its bounded size"
            ));
        }
        items.remove(0);
    }
    Ok(())
}

fn encoded_json_bytes(value: &impl serde::Serialize) -> Result<usize> {
    #[derive(Default)]
    struct ByteCount(usize);
    impl Write for ByteCount {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0 = self.0.saturating_add(buffer.len());
            Ok(buffer.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut count = ByteCount::default();
    serde_json::to_writer_pretty(&mut count, value)?;
    Ok(count.0.saturating_add(1)) // atomic_write_json's trailing LF
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        env::temp_dir().join(format!(
            "collection-store-{}-{nonce}-{sequence}",
            std::process::id()
        ))
    }

    #[test]
    fn round_trips_normalized_documents_and_paths() {
        let root = temp_root();
        let store = CollectionStore::new(root.clone()).unwrap();
        let missing = store.read_collection("agent-usage-reports").unwrap();
        assert_eq!(missing["schemaVersion"], STATE_SCHEMA_VERSION);
        assert_eq!(missing["collection"], "agent-usage-reports");
        assert_eq!(missing["items"], json!([]));

        let saved = store
            .write_collection(
                "agent-usage-reports",
                json!({"items": [{"generatedAt": "2026-07-01T00:00:00Z"}]}),
            )
            .unwrap();
        assert_eq!(store.read_collection("agent-usage-reports").unwrap(), saved);
        let path = store.collection_path("agent-usage-reports").unwrap();
        assert_eq!(path, root.join("agent-usage-reports.json"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&root).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_whitespace_only_existing_collection_is_malformed_not_empty() {
        let root = temp_root();
        let store = CollectionStore::new(root.clone()).unwrap();
        let path = store.collection_path("agent-usage-reports").unwrap();
        crate::fs::atomic_write_private_text_bounded(&path, "   \n", 16).unwrap();
        let error = store.read_collection("agent-usage-reports").unwrap_err();
        assert!(
            error.to_string().contains("malformed"),
            "a whitespace-only file is corrupt state, not an empty collection: {error}"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retention_keeps_the_complete_latest_item_under_a_byte_budget() {
        let one = json!({"items": [{"payload": "b"}]});
        let budget = encoded_json_bytes(&one).unwrap();
        let mut document = json!({"items": [{"payload": "a"}, {"payload": "b"}]});
        retain_latest_items(&mut document, 10, budget).unwrap();
        assert_eq!(document["items"], json!([{"payload": "b"}]));

        let mut document = json!({"items": [{"payload": "a"}, {"payload": "b"}]});
        retain_latest_items(&mut document, 1, 16 * 1024 * 1024).unwrap();
        assert_eq!(document["items"], json!([{"payload": "b"}]));

        let mut oversized = json!({"items": [{"payload": "much too large"}]});
        let error = retain_latest_items(&mut oversized, 10, 16).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("complete latest collection item exceeds its bounded size")
        );
        assert_eq!(oversized["items"], json!([{"payload": "much too large"}]));
    }

    #[test]
    fn retention_holds_a_multi_item_suffix_at_the_exact_byte_limit() {
        // A collection encoded to exactly the budget keeps every complete
        // item.
        let items = vec![
            json!({"payload": "a".repeat(40)}),
            json!({"payload": "b".repeat(60)}),
        ];
        let exact = json!({"items": items.clone()});
        let budget = encoded_json_bytes(&exact).unwrap();
        let mut document = exact.clone();
        retain_latest_items(&mut document, 10, budget).unwrap();
        assert_eq!(document["items"], json!(items));

        // One byte under sheds the oldest until the suffix truly fits —
        // never an outright refusal while a complete item still fits.
        let mut under = exact.clone();
        retain_latest_items(&mut under, 10, budget - 1).unwrap();
        assert_eq!(under["items"], json!([items[1].clone()]));
    }

    #[test]
    fn a_refused_oversized_write_never_replaces_the_previous_document() {
        let root = temp_root();
        let store = CollectionStore::new(root.clone()).unwrap();
        store
            .write_collection("agent-usage-reports", json!({"items": [{"n": 1}]}))
            .unwrap();
        let path = store.collection_path("agent-usage-reports").unwrap();
        let before = fs::read(&path).unwrap();
        let oversized = json!({"items": ["x".repeat(MAX_COLLECTION_DOCUMENT_BYTES + 1)]});
        let error = store
            .write_collection("agent-usage-reports", oversized)
            .unwrap_err();
        assert!(error.to_string().contains("exceeds its bounded size"));
        assert_eq!(fs::read(&path).unwrap(), before);
        let leftovers = fs::read_dir(&root)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .count();
        assert_eq!(leftovers, 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn collection_names_cannot_escape_the_store_directory() {
        let store = CollectionStore::new(temp_root()).unwrap();
        for collection in ["", "../escape", "a/b", "a b", "a.json"] {
            assert!(
                store.collection_path(collection).is_err(),
                "{collection} should be refused"
            );
        }
        assert_eq!(
            state_root_for_data_home(Path::new("/tmp/data")),
            PathBuf::from("/tmp/data/client-state")
        );
    }

    #[test]
    fn environment_roots_follow_precedence_and_refuse_placeholders() {
        use std::ffi::OsString;
        let lookup = |name: &str| -> Option<OsString> {
            match name {
                "LICOUP_HOME" => Some(OsString::from("/primary")),
                "LICOUP_PORTABLE_DIR" => Some(OsString::from("/secondary")),
                _ => None,
            }
        };
        assert_eq!(
            resolve_env_data_home(lookup, || Ok(PathBuf::from("/cwd"))).unwrap(),
            Some(PathBuf::from("/primary")),
            "LICOUP_HOME wins over LICOUP_PORTABLE_DIR"
        );
        let fallback = |name: &str| -> Option<OsString> {
            (name == "LICOUP_PORTABLE_DIR").then(|| OsString::from("/secondary"))
        };
        assert_eq!(
            resolve_env_data_home(fallback, || Ok(PathBuf::from("/cwd"))).unwrap(),
            Some(PathBuf::from("/secondary"))
        );
        for placeholder in ["", "   ", "$LICOUP_HOME", "${LICOUP_HOME}/x", "  $X "] {
            let only_placeholder = |name: &str| -> Option<OsString> {
                (name == "LICOUP_HOME").then(|| OsString::from(placeholder))
            };
            assert_eq!(
                resolve_env_data_home(only_placeholder, || Ok(PathBuf::from("/cwd"))).unwrap(),
                None,
                "{placeholder:?} is an unset placeholder, not a root"
            );
        }
        let relative = |name: &str| -> Option<OsString> {
            (name == "LICOUP_HOME").then(|| OsString::from("data/../portable"))
        };
        assert_eq!(
            resolve_env_data_home(relative, || Ok(PathBuf::from("/cwd"))).unwrap(),
            Some(PathBuf::from("/cwd/portable")),
            "a relative root is joined onto the current directory, normalized"
        );
    }

    #[test]
    fn saved_locator_requires_one_absolute_path() {
        assert_eq!(
            parse_saved_data_home("  /data/licoup \n").unwrap(),
            PathBuf::from("/data/licoup")
        );
        assert_eq!(
            parse_saved_data_home("/data/a/../b").unwrap(),
            PathBuf::from("/data/b")
        );
        for invalid in ["", "  \n", "relative/root", "$HOME/data", "${DIR}/data"] {
            assert!(
                parse_saved_data_home(invalid).is_err(),
                "{invalid:?} must be invalid, not silently relocated"
            );
        }
    }

    #[test]
    fn macos_data_volume_prefixes_strip_back_to_real_home() {
        assert_eq!(
            strip_macos_data_volume(Path::new("/System/Volumes/Data/example/home")),
            PathBuf::from("/example/home")
        );
        assert_eq!(
            strip_macos_data_volume(Path::new("/System/Volumes/Data")),
            PathBuf::from("/")
        );
        assert_eq!(
            strip_macos_data_volume(Path::new("/elsewhere/home")),
            PathBuf::from("/elsewhere/home")
        );
    }
}
