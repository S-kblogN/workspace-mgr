//! Native exact-version archive transport and immutable registry protocol.
//!
//! Receipts and private journals remain compatible with the 0.6.1 adapter.
//! Every mutation is authorized by the existing Git claims; S3 never supplies
//! an implicit retry for a write whose response may have been lost.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufReader, Write};
use std::path::Path;
use std::thread;
use std::time::Duration;

use base64::Engine;
use chrono::{DateTime, SecondsFormat, Utc};
use md5::Md5;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;
use crate::hex::encode_lower;
use crate::native_s3::{S3Client, S3Error, S3Response};
use crate::process;

const MAX_PAGES: usize = 100_000;
const COPY_LIMIT: u64 = 5 * (1 << 30);
const COPY_PART_SIZE: u64 = 512 * (1 << 20);
const READ_WORKERS: usize = 16;
const SOURCE_FIELDS: [&str; 9] = [
    "source_object",
    "destination_object",
    "source_version_id",
    "source_last_modified",
    "source_is_latest",
    "source_list_order",
    "delete_marker",
    "size",
    "source_etag",
];
const COPY_HEADERS: [&str; 14] = [
    "CacheControl",
    "ContentDisposition",
    "ContentEncoding",
    "ContentLanguage",
    "ContentType",
    "Expires",
    "WebsiteRedirectLocation",
    "StorageClass",
    "ServerSideEncryption",
    "SSEKMSKeyId",
    "BucketKeyEnabled",
    "ObjectLockMode",
    "ObjectLockRetainUntilDate",
    "ObjectLockLegalHoldStatus",
];
const PRIVATE_FIELDS: [&str; 5] = [
    "started",
    "multipart_upload_id",
    "cancel_started",
    "cancel_deleted",
    "cancel_owned_versions",
];

trait Storage {
    fn call(
        &self,
        operation: &str,
        args: &Value,
        body: Option<&[u8]>,
    ) -> std::result::Result<S3Response, S3Error>;
    fn bucket(&self) -> &str;
    fn prefix(&self) -> &str;
    fn b2(&self) -> bool;
    fn read_heads(&self, requests: &[Value]) -> Result<Vec<Value>> {
        requests
            .iter()
            .map(|args| Ok(self.call("head_object", args, None)?.value))
            .collect()
    }
    fn read_registry(&self, args: &Value) -> Result<Value> {
        let response = self.call("get_object", args, None)?;
        validate_registry_get(&response.value, args, response.body.len() as u64)?;
        serde_json::from_slice(&response.body)
            .map_err(|error| message(format!("invalid archive registry JSON: {error}")))
    }
    fn read_registries(&self, requests: &[Value], _limit: usize) -> Result<Vec<Value>> {
        requests
            .iter()
            .map(|args| self.read_registry(args))
            .collect()
    }
}

impl Storage for S3Client {
    fn call(
        &self,
        operation: &str,
        args: &Value,
        body: Option<&[u8]>,
    ) -> std::result::Result<S3Response, S3Error> {
        self.call_s3(operation, args, body)
    }
    fn bucket(&self) -> &str {
        &self.bucket
    }
    fn prefix(&self) -> &str {
        &self.prefix
    }
    fn b2(&self) -> bool {
        self.b2
    }
    fn read_heads(&self, requests: &[Value]) -> Result<Vec<Value>> {
        parallel_heads(self, requests)
    }
    fn read_registry(&self, args: &Value) -> Result<Value> {
        let scratch = tempfile::tempdir().at(std::env::temp_dir())?;
        let path = scratch.path().join("receipt.json");
        let response = self.get_to_file(args, &path)?;
        let input = fs::File::open(&path).at(&path)?;
        validate_registry_get(&response.value, args, input.metadata().at(&path)?.len())?;
        serde_json::from_reader(BufReader::new(input))
            .map_err(|error| message(format!("invalid archive registry JSON: {error}")))
    }
    fn read_registries(&self, requests: &[Value], limit: usize) -> Result<Vec<Value>> {
        parallel_registries(self, requests, limit)
    }
}

fn parallel_heads(store: &(impl Storage + Sync), requests: &[Value]) -> Result<Vec<Value>> {
    crate::native_versions::bounded_map_with_workers(
        requests,
        READ_WORKERS,
        false,
        |args: &Value| Ok(store.call("head_object", args, None)?.value),
    )
}

fn parallel_registries(
    store: &(impl Storage + Sync),
    requests: &[Value],
    limit: usize,
) -> Result<Vec<Value>> {
    crate::native_versions::bounded_map_with_workers(
        requests,
        limit.clamp(1, READ_WORKERS),
        false,
        |args: &Value| store.read_registry(args),
    )
}

fn checked_heads(store: &impl Storage, requests: &[Value]) -> Result<Vec<Value>> {
    let responses = store.read_heads(requests)?;
    if responses.len() != requests.len() {
        return Err(message(
            "archive HEAD batch returned an incomplete inventory",
        ));
    }
    Ok(responses)
}

fn validate_registry_get(info: &Value, request: &Value, count: u64) -> Result<()> {
    if info["VersionId"] != request["VersionId"]
        || info["DeleteMarker"] == true
        || info["delete_marker"] == true
    {
        return Err(message(
            "archive registry read did not return its exact requested payload version",
        ));
    }
    let size = info
        .get("ContentLength")
        .or_else(|| info.get("Size"))
        .and_then(Value::as_u64)
        .ok_or_else(|| message("archive registry GET returned no complete payload size"))?;
    if size != count
        || info
            .get("Size")
            .and_then(Value::as_u64)
            .is_some_and(|listed| listed != count)
    {
        return Err(message(
            "archive registry GET has a mismatched payload count",
        ));
    }
    Ok(())
}

fn message(text: impl Into<String>) -> Error {
    Error::message(text)
}
fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| message(format!("archive record has no {key}")))
}
fn rows(value: &Value) -> Result<&Vec<Value>> {
    value
        .get("versions")
        .and_then(Value::as_array)
        .ok_or_else(|| message("archive receipt contains no version inventory"))
}
fn object_mut(value: &mut Value) -> Result<&mut Map<String, Value>> {
    value
        .as_object_mut()
        .ok_or_else(|| message("archive record is not an object"))
}
fn canonical(value: &Value) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|error| message(error.to_string()))
}
fn digest(bytes: &[u8]) -> String {
    encode_lower(Sha256::digest(bytes))
}
fn etag(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(|value| value.trim_matches('"'))
        .filter(|value| !value.is_empty())
}
fn timestamp(value: &Value) -> Result<String> {
    let raw = value
        .as_str()
        .ok_or_else(|| message("S3 history contains no timezone-aware timestamp"))?;
    let date = DateTime::parse_from_rfc3339(raw)
        .map_err(|_| message("S3 history contains an invalid timestamp"))?
        .with_timezone(&Utc);
    let format = if date.timestamp_subsec_micros() == 0 {
        SecondsFormat::Secs
    } else {
        SecondsFormat::Micros
    };
    Ok(date.to_rfc3339_opts(format, false))
}
fn relative_path(raw: &str) -> Result<&str> {
    if raw.is_empty()
        || raw.starts_with('/')
        || raw
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(message(format!("invalid archive task path: {raw:?}")));
    }
    Ok(raw)
}
fn key(store: &impl Storage, object: &str) -> String {
    if store.prefix().is_empty() {
        object.to_owned()
    } else {
        format!("{}/{object}", store.prefix())
    }
}
fn context(store: &impl Storage, payload: &Value) -> Result<Value> {
    let source = relative_path(text(payload, "source")?)?;
    let destination = relative_path(text(payload, "destination")?)?;
    if source == destination
        || source.starts_with(&format!("{destination}/"))
        || destination.starts_with(&format!("{source}/"))
    {
        return Err(message(
            "archive source and destination task paths must not overlap",
        ));
    }
    Ok(
        json!({"schema_version":1,"remote":"workspace-mgr","bucket":store.bucket(),
              "remote_prefix":store.prefix(),"source":source,"destination":destination}),
    )
}
fn receipt_context(receipt: &Value) -> Value {
    let mut context: Value = [
        "schema_version",
        "remote",
        "bucket",
        "remote_prefix",
        "source",
        "destination",
    ]
    .into_iter()
    .map(|name| (name.to_owned(), receipt[name].clone()))
    .collect::<Map<String, Value>>()
    .into();
    if supported_copy_schema(receipt) {
        context["schema_version"] = 1.into();
    }
    context
}
fn supported_copy_schema(receipt: &Value) -> bool {
    receipt["schema_version"] == 1
        || receipt["schema_version"] == crate::policy::ARCHIVE_COPY_JOURNAL_SCHEMA_VERSION
}
fn validate_receipt(receipt: &Value, context: &Value) -> Result<()> {
    if !receipt.is_object() || receipt_context(receipt) != *context {
        return Err(message(
            "archive receipt does not match its source, destination, or remote",
        ));
    }
    let source = text(context, "source")?;
    let destination = text(context, "destination")?;
    let mut sources = BTreeSet::new();
    let mut destinations = BTreeSet::new();
    for row in rows(receipt)? {
        if !row.is_object() || SOURCE_FIELDS.iter().any(|name| row.get(*name).is_none()) {
            return Err(message(
                "archive receipt contains incomplete version records",
            ));
        }
        let old = text(row, "source_object")?;
        let suffix = old
            .strip_prefix(&format!("{source}/"))
            .ok_or_else(|| message("archive receipt escaped its source task"))?;
        if row["destination_object"] != format!("{destination}/{suffix}") {
            return Err(message("archive receipt escaped its destination task"));
        }
        if !sources.insert((old, text(row, "source_version_id")?)) {
            return Err(message(
                "archive receipt contains missing or repeated source versions",
            ));
        }
        timestamp(&row["source_last_modified"])?;
        if !row["source_is_latest"].is_boolean()
            || !row["delete_marker"].is_boolean()
            || row["source_list_order"].as_u64().is_none()
        {
            return Err(message(
                "archive receipt contains invalid version chronology",
            ));
        }
        if row["delete_marker"] == true {
            if !row["size"].is_null() || !row["source_etag"].is_null() {
                return Err(message(
                    "archive receipt gives payload metadata to a delete marker",
                ));
            }
        } else if row["size"].as_u64().is_none() || etag(&row["source_etag"]).is_none() {
            return Err(message(
                "archive receipt contains incomplete payload metadata",
            ));
        }
        if !row["destination_version_id"].is_null() {
            let version = text(row, "destination_version_id")?;
            if version == "null"
                || !destinations.insert((text(row, "destination_object")?, version))
            {
                return Err(message(
                    "archive receipt contains missing or repeated destination versions",
                ));
            }
        }
    }
    Ok(())
}
fn load_journal(path: &Path) -> Result<Option<Value>> {
    if !path.is_absolute() || path.is_symlink() {
        return Err(message(
            "archive journal must be an absolute regular-file path",
        ));
    }
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(Error::Io {
                path: path.to_owned(),
                source,
            });
        }
    };
    if !path.is_file() {
        return Err(message("archive journal must be a regular file"));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| message(format!("invalid archive journal: {error}")))
}
fn load_copy_journal(path: &Path) -> Result<Option<Value>> {
    let journal = load_journal(path)?;
    if journal
        .as_ref()
        .is_some_and(|value| !supported_copy_schema(value))
    {
        return Err(message(
            "private archive copy journal has an unsupported schema",
        ));
    }
    Ok(journal)
}
fn save_journal(path: &Path, journal: &Value) -> Result<()> {
    if !path.is_absolute() || path.is_symlink() {
        return Err(message(
            "archive journal must be an absolute regular-file path",
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| message("archive journal has no parent"))?;
    if !supported_copy_schema(journal) {
        return Err(message(
            "private archive copy journal has an unsupported schema",
        ));
    }
    let mut upgraded = journal.clone();
    upgraded["schema_version"] = crate::policy::ARCHIVE_COPY_JOURNAL_SCHEMA_VERSION.into();
    fs::create_dir_all(parent).at(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).at(parent)?;
    let mut body =
        serde_json::to_vec_pretty(&upgraded).map_err(|error| message(error.to_string()))?;
    body.push(b'\n');
    temporary.write_all(&body).at(path)?;
    temporary.as_file().sync_all().at(path)?;
    temporary.persist(path).map_err(|error| Error::Io {
        path: path.to_owned(),
        source: error.error,
    })?;
    fs::File::open(parent).at(parent)?.sync_all().at(parent)?;
    Ok(())
}
fn list_versions(store: &impl Storage, prefix: &str) -> Result<Vec<Value>> {
    let mut request = json!({"Bucket":store.bucket(),"Prefix":prefix,"MaxKeys":1000});
    let mut result = Vec::new();
    let mut seen = BTreeSet::new();
    let mut markers = BTreeSet::new();
    for _ in 0..MAX_PAGES {
        let response = store.call("list_object_versions", &request, None)?.value;
        for section in ["Versions", "DeleteMarkers"] {
            if let Some(items) = response.get(section).and_then(Value::as_array) {
                for item in items {
                    let name = text(item, "Key")?;
                    let version = text(item, "VersionId")?;
                    if !name.starts_with(prefix)
                        || !seen.insert((name.to_owned(), version.to_owned()))
                    {
                        return Err(message(
                            "S3 history listing escaped its prefix or repeated an object version",
                        ));
                    }
                    let mut item = item.clone();
                    item["delete_marker"] = (section == "DeleteMarkers").into();
                    result.push(item);
                }
            }
        }
        if response["IsTruncated"] != true {
            return Ok(result);
        }
        let marker = (
            text(&response, "NextKeyMarker")?.to_owned(),
            text(&response, "NextVersionIdMarker")?.to_owned(),
        );
        if !markers.insert(marker.clone()) {
            return Err(message(
                "S3 history listing has missing or repeated pagination markers",
            ));
        }
        request["KeyMarker"] = marker.0.into();
        request["VersionIdMarker"] = marker.1.into();
    }
    Err(message(
        "S3 history listing exceeded the archive pagination limit",
    ))
}
fn destination_inventory(store: &impl Storage, context: &Value) -> Result<Vec<Value>> {
    list_versions(
        store,
        &key(store, &format!("{}/", text(context, "destination")?)),
    )
}
fn source_inventory(store: &impl Storage, context: &Value) -> Result<Vec<Value>> {
    let source = text(context, "source")?;
    let destination = text(context, "destination")?;
    let prefix = key(store, &format!("{source}/"));
    let mut rows = Vec::new();
    let mut latest = BTreeMap::<String, (usize, usize)>::new();
    for (index, item) in list_versions(store, &prefix)?.into_iter().enumerate() {
        let suffix = text(&item, "Key")?
            .strip_prefix(&prefix)
            .ok_or_else(|| message("S3 history escaped source task"))?;
        let deleted = item["delete_marker"] == true;
        let size = if deleted {
            Value::Null
        } else {
            item["Size"].clone()
        };
        let source_etag = if deleted {
            Value::Null
        } else {
            etag(&item["ETag"]).map(Value::from).unwrap_or(Value::Null)
        };
        if !deleted && (size.as_u64().is_none() || source_etag.is_null()) {
            return Err(message("S3 history contains incomplete payload metadata"));
        }
        let object = format!("{source}/{suffix}");
        let group = latest.entry(object.clone()).or_default();
        group.0 += 1;
        group.1 += usize::from(item["IsLatest"] == true);
        rows.push(json!({"source_object":object,"destination_object":format!("{destination}/{suffix}"),
            "source_version_id":item["VersionId"],"destination_version_id":null,
            "source_last_modified":timestamp(&item["LastModified"])? ,"source_is_latest":item["IsLatest"] == true,
            "source_list_order":index,"delete_marker":deleted,"size":size,"source_etag":source_etag,"destination_etag":null}));
    }
    if latest.values().any(|(_, latest)| *latest != 1) {
        return Err(message(
            "S3 history does not identify exactly one current version per object",
        ));
    }
    rows.sort_by(|left, right| {
        left["source_object"]
            .as_str()
            .cmp(&right["source_object"].as_str())
            .then_with(|| {
                (left["source_is_latest"] == true).cmp(&(right["source_is_latest"] == true))
            })
            .then_with(|| {
                left["source_last_modified"]
                    .as_str()
                    .cmp(&right["source_last_modified"].as_str())
            })
            .then_with(|| {
                right["source_list_order"]
                    .as_u64()
                    .cmp(&left["source_list_order"].as_u64())
            })
    });
    Ok(rows)
}
fn source_signature(rows: &[Value]) -> Value {
    rows.iter()
        .map(|row| {
            SOURCE_FIELDS
                .into_iter()
                .map(|name| (name.to_owned(), row[name].clone()))
                .collect::<Map<String, Value>>()
                .into()
        })
        .collect::<Vec<Value>>()
        .into()
}
fn token(journal: &Value, row: &Value) -> Result<String> {
    let rendered = serde_json::to_string(&json!([
        text(journal, "transaction_id")?,
        text(row, "source_object")?,
        text(row, "source_version_id")?
    ]))
    .map_err(|error| message(error.to_string()))?;
    // The 0.6.1 Python journal uses json.dumps(..., ensure_ascii=True)
    // for this token alone. Preserve those bytes, including UTF-16 pairs,
    // so existing copies remain recoverable and cancellable.
    let mut ascii = String::with_capacity(rendered.len());
    for character in rendered.chars() {
        if character <= '\u{7e}' {
            ascii.push(character);
        } else {
            let mut buffer = [0u16; 2];
            for code in character.encode_utf16(&mut buffer) {
                ascii.push_str(&format!("\\u{code:04x}"));
            }
        }
    }
    Ok(digest(ascii.as_bytes()))
}
fn metadata_key(journal: &Value) -> Result<String> {
    // B2 file-info names are limited to 50 UTF-8 bytes and lowercased by
    // the provider. This selector is 50 ASCII bytes; the value still carries
    // the complete transaction/source-version token, never a truncated proof.
    let transaction = digest(text(journal, "transaction_id")?.as_bytes());
    Ok(format!("wm-ac-{}", &transaction[..44]))
}
fn legacy_metadata_key(journal: &Value) -> Result<String> {
    Ok(format!(
        "workspace-mgr-archive-copy-{}",
        text(journal, "transaction_id")?
    ))
}
fn metadata_owned(info: &Value, compact: &str, legacy: &str, expected: &str) -> bool {
    let Some(metadata) = info["Metadata"].as_object() else {
        return false;
    };
    let mut found = false;
    for (name, value) in metadata {
        if name.eq_ignore_ascii_case(compact) || name.eq_ignore_ascii_case(legacy) {
            if value.as_str() != Some(expected) {
                return false;
            }
            found = true;
        }
    }
    found
}
fn public_receipt(journal: &Value, status: Option<&str>) -> Result<Value> {
    if !supported_copy_schema(journal) {
        return Err(message(
            "private archive copy journal has an unsupported schema",
        ));
    }
    let mut receipt = journal.clone();
    receipt["schema_version"] = 1.into();
    for row in receipt["versions"]
        .as_array_mut()
        .ok_or_else(|| message("archive journal has no rows"))?
    {
        for name in PRIVATE_FIELDS {
            object_mut(row)?.remove(name);
        }
    }
    if let Some(status) = status {
        receipt["status"] = status.into();
    }
    receipt["source_cleanup"] = "after_verified_git_publication".into();
    Ok(receipt)
}
fn head_payload(
    store: &impl Storage,
    object: &str,
    version: &str,
    size: &Value,
    expected_etag: &Value,
) -> Result<Value> {
    let info = store
        .call(
            "head_object",
            &json!({"Bucket":store.bucket(),"Key":key(store,object),"VersionId":version}),
            None,
        )?
        .value;
    validate_payload_head(&info, object, version, size, expected_etag)?;
    Ok(info)
}
fn validate_payload_head(
    info: &Value,
    object: &str,
    version: &str,
    size: &Value,
    expected_etag: &Value,
) -> Result<()> {
    if info["VersionId"] != version
        || info["DeleteMarker"] == true
        || info["ContentLength"] != *size
        || etag(&info["ETag"]) != etag(expected_etag)
    {
        return Err(message(format!(
            "archive payload version is missing or mismatched: {object:?}"
        )));
    }
    Ok(())
}
fn cached_version_head<'a>(
    store: &impl Storage,
    object: &str,
    version: &str,
    heads: &'a mut BTreeMap<(String, String), Value>,
) -> Result<&'a Value> {
    use std::collections::btree_map::Entry;
    let id = (object.to_owned(), version.to_owned());
    // A legacy null generation can be overwritten. It is not an immutable
    // version and must retain a fresh HEAD on each ownership comparison.
    if version == "null" {
        heads.remove(&id);
    }
    Ok(match heads.entry(id) {
        Entry::Occupied(entry) => entry.into_mut(),
        Entry::Vacant(entry) => entry.insert(
            store
                .call(
                    "head_object",
                    &json!({"Bucket":store.bucket(),"Key":object,"VersionId":version}),
                    None,
                )?
                .value,
        ),
    })
}
fn record_destination(
    row: &mut Value,
    version: &Value,
    destination_etag: &Value,
    modified: &Value,
) -> Result<()> {
    let version = version
        .as_str()
        .filter(|value| !value.is_empty() && *value != "null")
        .ok_or_else(|| message("archive copy returned no exact destination version ID"))?;
    let destination_etag = if row["delete_marker"] == true {
        Value::Null
    } else {
        etag(destination_etag)
            .map(Value::from)
            .ok_or_else(|| message("archive copy returned no destination ETag"))?
    };
    row["destination_version_id"] = version.into();
    row["destination_etag"] = destination_etag;
    if !modified.is_null() {
        row["destination_last_modified"] = timestamp(modified)?.into();
    }
    for name in ["started", "multipart_upload_id"] {
        object_mut(row)?.remove(name);
    }
    Ok(())
}
fn destination_uploads(store: &impl Storage, context: &Value) -> Result<Vec<Value>> {
    let prefix = key(store, &format!("{}/", text(context, "destination")?));
    let mut request = json!({"Bucket":store.bucket(),"Prefix":prefix,"MaxUploads":1000});
    let mut uploads = Vec::new();
    let mut seen = BTreeSet::new();
    let mut markers = BTreeSet::new();
    for _ in 0..MAX_PAGES {
        let response = store.call("list_multipart_uploads", &request, None)?.value;
        if let Some(items) = response["Uploads"].as_array() {
            for item in items {
                let object = text(item, "Key")?;
                let upload = text(item, "UploadId")?;
                if !object.starts_with(&prefix)
                    || !seen.insert((object.to_owned(), upload.to_owned()))
                {
                    return Err(message(
                        "archive multipart inventory contains an escaped or repeated upload",
                    ));
                }
                uploads.push(json!({"key":object,"upload_id":upload}));
            }
        }
        if response["IsTruncated"] != true {
            return Ok(uploads);
        }
        let marker = (
            text(&response, "NextKeyMarker")?.to_owned(),
            text(&response, "NextUploadIdMarker")?.to_owned(),
        );
        if !markers.insert(marker.clone()) {
            return Err(message(
                "archive multipart inventory has missing or repeated pagination markers",
            ));
        }
        request["KeyMarker"] = marker.0.into();
        request["UploadIdMarker"] = marker.1.into();
    }
    Err(message(
        "archive multipart inventory exceeded the archive pagination limit",
    ))
}

fn git(repo: &GitRepo, args: &[&str]) -> Result<Vec<u8>> {
    Ok(process::run_bytes("git", args, &repo.root, &BTreeMap::new(), None, true)?.stdout)
}
fn valid_oid(raw: &str) -> bool {
    [40, 64].contains(&raw.len())
        && raw
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
fn verify_reservation(
    repo: &GitRepo,
    context: &Value,
    payload: &Value,
    journal: Option<&Value>,
) -> Result<()> {
    let proof = payload
        .get("reservation")
        .ok_or_else(|| message("archive copy requires a valid Git copy reservation"))?;
    if proof["mode"] != "git-copy-reservation" {
        return Err(message(
            "archive copy requires a valid Git copy reservation",
        ));
    }
    let receipt = &proof["receipt"];
    validate_receipt(receipt, context)?;
    if crate::archive_reservation::normalized_receipt(receipt)? != *receipt {
        return Err(message(
            "archive copy reservation does not contain a normalized planned receipt",
        ));
    }
    if let Some(planned) = payload.get("planned")
        && crate::archive_reservation::normalized_receipt(planned)? != *receipt
    {
        return Err(message(
            "archive copy reservation selects another planned receipt",
        ));
    }
    if let Some(journal) = journal
        && source_signature(rows(journal)?) != source_signature(rows(receipt)?)
    {
        return Err(message(
            "archive copy reservation selects another original source snapshot",
        ));
    }
    let root = text(proof, "repo_path")?;
    let canonical_root = repo.root.canonicalize().at(&repo.root)?;
    if canonical_root.to_str() != Some(root)
        || !Path::new(root).is_absolute()
        || payload.get("repo_path").is_some_and(|value| value != root)
    {
        return Err(message(
            "archive copy reservation selects another repository",
        ));
    }
    let owner = digest(root.as_bytes());
    let nonce = text(proof, "attempt_nonce")?;
    let body = canonical(&json!({"attempt_nonce":nonce,"owner_hash":owner,"receipt":receipt}))?;
    let identity = canonical(&json!([
        context["bucket"],
        context["remote_prefix"],
        context["source"]
    ]))?;
    let reference = format!("refs/tags/workspace-mgr/archive-copy/{}", digest(&identity));
    let oid = text(proof, "oid")?;
    let remote = text(proof, "remote")?;
    if proof["owner_hash"] != owner
        || proof["ref"] != reference
        || proof["descriptor_sha256"] != digest(&body)
        || proof["receipt_sha256"] != digest(&canonical(receipt)?)
        || !valid_oid(oid)
        || remote.starts_with('-')
        || remote.chars().any(char::is_control)
    {
        return Err(message(
            "archive copy reservation has an invalid immutable identity",
        ));
    }
    let state = load_journal(Path::new(text(proof, "state_path")?))?
        .ok_or_else(|| message("archive copy reservation has no private journal"))?;
    if state["schema_version"] != 1
        || state["acquired"] != true
        || state["attempt_nonce"] != nonce
        || state["owner_hash"] != owner
        || state["receipt"] != *receipt
    {
        return Err(message(
            "archive copy reservation is not owned by its acquired private journal",
        ));
    }
    if Config::load(repo)?.git.remote != remote {
        return Err(message(
            "archive copy reservation selects another configured Git remote",
        ));
    }
    let fetch = git(repo, &["remote", "get-url", "--all", remote])?;
    let push = git(repo, &["remote", "get-url", "--push", "--all", remote])?;
    if fetch
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .count()
        != 1
        || push != fetch
    {
        return Err(message(
            "archive copy reservation requires one identical Git fetch and push destination",
        ));
    }
    if git(repo, &["ls-remote", "--refs", "--", remote, &reference])?
        != format!("{oid}\t{reference}\n").as_bytes()
    {
        return Err(message(
            "archive copy reservation Git claim was removed or replaced",
        ));
    }
    if crate::archive_git_control::read_body(&repo.root, oid)?.as_bytes() != body {
        return Err(message(
            "archive copy reservation Git control body differs from its descriptor",
        ));
    }
    Ok(())
}

fn validate_destination(
    store: &impl Storage,
    context: &Value,
    journal: &mut Value,
    recover: bool,
    allow_additions: bool,
) -> Result<Vec<Value>> {
    let mut actual = destination_inventory(store, context)?;
    let mut wanted = BTreeMap::<(String, String), usize>::new();
    let mut pending = Vec::new();
    for (index, row) in rows(journal)?.iter().enumerate() {
        if let Some(version) = row["destination_version_id"].as_str() {
            wanted.insert(
                (
                    key(store, text(row, "destination_object")?),
                    version.to_owned(),
                ),
                index,
            );
        } else if row["started"] == true {
            pending.push(index);
        }
    }
    if allow_additions {
        // A renamed task stays active. Later publications may add immutable
        // versions at its new path; only the signed migration's exact copied
        // history belongs to this retirement verification.
        actual.retain(|item| {
            wanted.contains_key(&(
                item["Key"].as_str().unwrap_or("").to_owned(),
                item["VersionId"].as_str().unwrap_or("").to_owned(),
            ))
        });
    }
    let unknown = actual
        .iter()
        .filter(|item| {
            !wanted.contains_key(&(
                item["Key"].as_str().unwrap_or("").to_owned(),
                item["VersionId"].as_str().unwrap_or("").to_owned(),
            ))
        })
        .collect::<Vec<_>>();
    let mut recovered = false;
    if recover && unknown.len() == 1 && pending.len() == 1 {
        let index = pending[0];
        let row = &journal["versions"][index];
        let item = unknown[0];
        if item["Key"] == key(store, text(row, "destination_object")?)
            && item["delete_marker"] == row["delete_marker"]
        {
            let owned = if row["delete_marker"] == true {
                true
            } else {
                let info = head_payload(
                    store,
                    text(row, "destination_object")?,
                    text(item, "VersionId")?,
                    &row["size"],
                    &item["ETag"],
                )?;
                metadata_owned(
                    &info,
                    &metadata_key(journal)?,
                    &legacy_metadata_key(journal)?,
                    &token(journal, row)?,
                )
            };
            if owned {
                wanted.insert(
                    (
                        text(item, "Key")?.to_owned(),
                        text(item, "VersionId")?.to_owned(),
                    ),
                    index,
                );
                record_destination(
                    &mut journal["versions"][index],
                    &item["VersionId"],
                    &item["ETag"],
                    &item["LastModified"],
                )?;
                recovered = true;
            }
        }
    }
    if !unknown.is_empty() && !recovered {
        return Err(message(
            "archive destination contains unrelated or ambiguous object history",
        ));
    }
    if actual.len() != wanted.len() {
        return Err(message(
            "archive destination lost a previously copied object version",
        ));
    }
    for chunk in actual.chunks(READ_WORKERS) {
        let mut indices = Vec::new();
        let mut payloads = Vec::new();
        let mut requests = Vec::new();
        for item in chunk {
            let index = *wanted
                .get(&identity(item)?)
                .ok_or_else(|| message("archive destination contains an unknown version"))?;
            let row = &journal["versions"][index];
            if item["delete_marker"] != row["delete_marker"] {
                return Err(message(
                    "archive destination has a mismatched delete marker",
                ));
            }
            indices.push(index);
            if row["delete_marker"] != true {
                payloads.push(index);
                requests.push(json!({"Bucket":store.bucket(),
                    "Key":key(store,text(row,"destination_object")?),
                    "VersionId":text(row,"destination_version_id")?}));
            }
        }
        for (index, info) in payloads.into_iter().zip(checked_heads(store, &requests)?) {
            let row = &journal["versions"][index];
            validate_payload_head(
                &info,
                text(row, "destination_object")?,
                text(row, "destination_version_id")?,
                &row["size"],
                &row["destination_etag"],
            )?;
        }
        for (index, item) in indices.into_iter().zip(chunk) {
            journal["versions"][index]["destination_last_modified"] =
                timestamp(&item["LastModified"])?.into();
        }
    }
    Ok(actual)
}
fn verify_history_with(store: &impl Storage, context: &Value, journal: &mut Value) -> Result<()> {
    verify_history_mode(store, context, journal, false)
}
fn verify_history_mode(
    store: &impl Storage,
    context: &Value,
    journal: &mut Value,
    allow_additions: bool,
) -> Result<()> {
    if rows(journal)?.iter().any(|row| {
        row["destination_version_id"]
            .as_str()
            .is_none_or(str::is_empty)
    }) {
        return Err(message("archive history copy is incomplete"));
    }
    let actual = validate_destination(store, context, journal, false, allow_additions)?;
    if allow_additions {
        return Ok(());
    }
    let latest = actual
        .iter()
        .filter(|item| item["IsLatest"] == true)
        .map(|item| (item["Key"].clone(), item["VersionId"].clone()))
        .collect::<Vec<_>>();
    let mut expected = rows(journal)?
        .iter()
        .filter(|row| row["source_is_latest"] == true)
        .map(|row| {
            Ok((
                key(store, text(row, "destination_object")?).into(),
                row["destination_version_id"].clone(),
            ))
        })
        .collect::<Result<Vec<(Value, Value)>>>()?;
    let mut latest = latest;
    latest.sort_by_key(|item| canonical(&json!(item)).unwrap_or_default());
    expected.sort_by_key(|item| canonical(&json!(item)).unwrap_or_default());
    if latest != expected {
        return Err(message(
            "archive destination current versions differ from the source snapshot",
        ));
    }
    Ok(())
}
fn verify_receipt_history_with(
    store: &impl Storage,
    context: &Value,
    receipt: &mut Value,
) -> Result<()> {
    let allow_additions = validated_rename_receipt(receipt)?;
    verify_history_mode(store, context, receipt, allow_additions)
}
fn validated_rename_receipt(receipt: &Value) -> Result<bool> {
    if receipt.get("migration_kind").is_none() {
        return Ok(false);
    }
    // A discriminator alone cannot turn an archive into an active-task
    // exception. Bind it to the task's immutable ID/timestamp and the exact
    // top-level old/new namespace mapping before accepting later versions.
    let path = format!(
        "{}/{}",
        text(receipt, "destination")?,
        crate::archive_migration::RECEIPT_NAME,
    );
    crate::archive_migration::validate(&path, receipt)?;
    Ok(crate::archive_migration::is_task_rename(receipt))
}
pub(crate) fn verify_history(client: &S3Client, receipt: &Value) -> Result<()> {
    let context = context(client, receipt)?;
    validate_receipt(receipt, &context)?;
    verify_receipt_history_with(client, &context, &mut receipt.clone())
}
fn verify_cancel_source_with(store: &impl Storage, context: &Value, receipt: &Value) -> Result<()> {
    validate_receipt(receipt, context)?;
    let current = source_inventory(store, context)?;
    let originals = current
        .iter()
        .map(|item| {
            Ok((
                (
                    text(item, "source_object")?,
                    text(item, "source_version_id")?,
                ),
                item,
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    for chunk in rows(receipt)?.chunks(READ_WORKERS) {
        let mut payloads = Vec::new();
        let mut requests = Vec::new();
        for row in chunk {
            let original =
                originals.get(&(text(row, "source_object")?, text(row, "source_version_id")?));
            if original.is_none_or(|item| {
                [
                    "source_last_modified",
                    "delete_marker",
                    "size",
                    "source_etag",
                ]
                .iter()
                .any(|name| item[*name] != row[*name])
            }) {
                return Err(message(
                    "archive cancellation requires every unchanged original source version and delete marker",
                ));
            }
            if row["delete_marker"] != true {
                payloads.push(row);
                requests.push(
                    json!({"Bucket":store.bucket(),"Key":key(store,text(row,"source_object")?),
                "VersionId":text(row,"source_version_id")?}),
                );
            }
        }
        for (row, info) in payloads.into_iter().zip(checked_heads(store, &requests)?) {
            validate_payload_head(
                &info,
                text(row, "source_object")?,
                text(row, "source_version_id")?,
                &row["size"],
                &row["source_etag"],
            )?;
        }
    }
    Ok(())
}
fn abort_upload(
    store: &impl Storage,
    row: &mut Value,
    authorize: impl FnOnce() -> Result<()>,
) -> Result<()> {
    if let Some(upload) = row["multipart_upload_id"].as_str() {
        authorize()?;
        match store.call("abort_multipart_upload",&json!({"Bucket":store.bucket(),"Key":key(store,text(row,"destination_object")?),"UploadId":upload}),None) {
            Ok(_) => (), Err(error) if error.code == "NoSuchUpload" => (), Err(error) => return Err(error.into()),
        }
        object_mut(row)?.remove("multipart_upload_id");
    }
    Ok(())
}
fn copy_payload(
    store: &impl Storage,
    repo: &GitRepo,
    context: &Value,
    payload: &Value,
    journal: &mut Value,
    index: usize,
    state: &Path,
) -> Result<()> {
    let row = journal["versions"][index].clone();
    let info = head_payload(
        store,
        text(&row, "source_object")?,
        text(&row, "source_version_id")?,
        &row["size"],
        &row["source_etag"],
    )?;
    let mut headers = Map::new();
    for field in COPY_HEADERS {
        if let Some(value) = info.get(field) {
            headers.insert(field.to_owned(), value.clone());
        }
    }
    let mut metadata = info["Metadata"].as_object().cloned().unwrap_or_default();
    let metadata_key = metadata_key(journal)?;
    let legacy_key = legacy_metadata_key(journal)?;
    if metadata.keys().any(|name| {
        name.eq_ignore_ascii_case(&metadata_key) || name.eq_ignore_ascii_case(&legacy_key)
    }) {
        return Err(message(
            "archive transaction metadata would replace existing source metadata",
        ));
    }
    metadata.insert(metadata_key, token(journal, &row)?.into());
    headers.insert("Metadata".to_owned(), metadata.into());
    let source = json!({"Bucket":store.bucket(),"Key":key(store,text(&row,"source_object")?),"VersionId":row["source_version_id"]});
    let destination = key(store, text(&row, "destination_object")?);
    if row["size"]
        .as_u64()
        .ok_or_else(|| message("archive payload has no size"))?
        <= COPY_LIMIT
    {
        let mut request = Value::Object(headers);
        request["Bucket"] = store.bucket().into();
        request["Key"] = destination.into();
        request["CopySource"] = source;
        request["CopySourceIfMatch"] = info["ETag"].clone();
        request["MetadataDirective"] = "REPLACE".into();
        request["TaggingDirective"] = "COPY".into();
        verify_reservation(repo, context, payload, Some(journal))?;
        let response = store.call("copy_object", &request, None)?.value;
        return record_destination(
            &mut journal["versions"][index],
            &response["VersionId"],
            &response["CopyObjectResult"]["ETag"],
            &response["CopyObjectResult"]["LastModified"],
        );
    }
    let tags = store.call("get_object_tagging",&json!({"Bucket":store.bucket(),"Key":key(store,text(&row,"source_object")?),"VersionId":row["source_version_id"]}),None)?.value;
    if let Some(tags) = tags["TagSet"].as_array().filter(|tags| !tags.is_empty()) {
        let mut encoded = url::form_urlencoded::Serializer::new(String::new());
        for item in tags {
            encoded.append_pair(
                text(item, "Key")?,
                item["Value"]
                    .as_str()
                    .ok_or_else(|| message("archive source contains an invalid tag value"))?,
            );
        }
        headers.insert("Tagging".to_owned(), encoded.finish().into());
    }
    let mut request = Value::Object(headers);
    request["Bucket"] = store.bucket().into();
    request["Key"] = destination.clone().into();
    verify_reservation(repo, context, payload, Some(journal))?;
    let created = store.call("create_multipart_upload", &request, None)?.value;
    let upload = text(&created, "UploadId")?.to_owned();
    journal["versions"][index]["multipart_upload_id"] = upload.clone().into();
    save_journal(state, journal)?;
    let size = row["size"]
        .as_u64()
        .ok_or_else(|| message("archive payload has no size"))?;
    let part_size = COPY_PART_SIZE.max(size.div_ceil(10_000));
    let copying = (|| -> Result<()> {
        if part_size > COPY_LIMIT {
            return Err(message(
                "archive object exceeds the S3 multipart-copy limit",
            ));
        }
        let mut parts = Vec::new();
        for (index, start) in (0..size).step_by(part_size as usize).enumerate() {
            let last = (start + part_size).min(size) - 1;
            verify_reservation(repo, context, payload, Some(journal))?;
            let response = store.call("upload_part_copy",&json!({"Bucket":store.bucket(),"Key":destination,"UploadId":upload,
                "PartNumber":index+1,"CopySource":source,"CopySourceIfMatch":info["ETag"],"CopySourceRange":format!("bytes={start}-{last}")}),None)?.value;
            parts.push(
                json!({"PartNumber":index+1,"ETag":text(&response["CopyPartResult"],"ETag")?}),
            );
        }
        verify_reservation(repo, context, payload, Some(journal))?;
        let completed = store.call("complete_multipart_upload",&json!({"Bucket":store.bucket(),"Key":destination,"UploadId":upload,"MultipartUpload":{"Parts":parts}}),None)?.value;
        record_destination(
            &mut journal["versions"][index],
            &completed["VersionId"],
            &completed["ETag"],
            &Value::Null,
        )
    })();
    if let Err(error) = copying {
        let mut row = journal["versions"][index].clone();
        abort_upload(store, &mut row, || {
            verify_reservation(repo, context, payload, Some(journal))
        })?;
        journal["versions"][index] = row;
        save_journal(state, journal)?;
        return Err(error);
    }
    Ok(())
}

#[derive(Clone)]
struct OwnedVersion {
    row: usize,
    record: usize,
    key: String,
    version: String,
    marker: bool,
}
fn identity(item: &Value) -> Result<(String, String)> {
    Ok((
        text(item, "Key")?.to_owned(),
        text(item, "VersionId")?.to_owned(),
    ))
}
fn cancel_inventory(
    store: &impl Storage,
    context: &Value,
    journal: &mut Value,
    verify_originals: bool,
) -> Result<(Vec<OwnedVersion>, Vec<Value>)> {
    text(journal, "transaction_id")?;
    let metadata = metadata_key(journal)?;
    let legacy_metadata = legacy_metadata_key(journal)?;
    let actual = destination_inventory(store, context)?;
    let mut actual_by_id = BTreeMap::new();
    let mut actual_by_key = BTreeMap::<String, Vec<usize>>::new();
    for (index, item) in actual.iter().enumerate() {
        let id = identity(item)?;
        actual_by_key.entry(id.0.clone()).or_default().push(index);
        actual_by_id.insert(id, index);
    }
    // Exact-version payload metadata is immutable. Reuse responses only in
    // this read-only inventory pass, checking each row's ownership and payload
    // expectations again. Every resume or post-delete inventory starts fresh.
    let mut heads = BTreeMap::new();
    let payload_keys = rows(journal)?
        .iter()
        .filter(|row| row["delete_marker"] != true)
        .map(|row| Ok(key(store, text(row, "destination_object")?)))
        .collect::<Result<BTreeSet<_>>>()?;
    let requests = actual
        .iter()
        .filter(|item| item["delete_marker"] != true && item["VersionId"] != "null")
        .filter(|item| {
            item["Key"]
                .as_str()
                .is_some_and(|key| payload_keys.contains(key))
        })
        .map(
            |item| json!({"Bucket":store.bucket(),"Key":item["Key"],"VersionId":item["VersionId"]}),
        )
        .collect::<Vec<_>>();
    for chunk in requests.chunks(READ_WORKERS) {
        for (request, response) in chunk.iter().zip(checked_heads(store, chunk)?) {
            heads.insert(identity(request)?, response);
        }
    }
    let mut tracked = BTreeSet::new();
    let mut owned = Vec::new();
    let mut owned_ids = BTreeSet::new();
    for index in 0..rows(journal)?.len() {
        let row_token = token(journal, &journal["versions"][index])?;
        let row = &mut journal["versions"][index];
        let version = row["destination_version_id"].as_str().map(str::to_owned);
        if row.get("cancel_owned_versions").is_none() {
            row["cancel_owned_versions"] = json!([]);
        }
        let records = row["cancel_owned_versions"].as_array().ok_or_else(|| {
            message("archive cancellation contains an invalid private version inventory")
        })?;
        if let Some(version) = &version
            && !records
                .iter()
                .any(|record| record["version_id"] == *version)
        {
            let record = json!({"version_id":version,"etag":row["destination_etag"],"delete_marker":row["delete_marker"],"started":row["cancel_started"] == true,"deleted":row["cancel_deleted"] == true});
            row["cancel_owned_versions"]
                .as_array_mut()
                .ok_or_else(|| message("invalid cancel inventory"))?
                .push(record);
        }
        let records = row["cancel_owned_versions"]
            .as_array()
            .ok_or_else(|| message("invalid cancel inventory"))?;
        for (record_index, record) in records.iter().enumerate() {
            let recorded = text(record, "version_id")?;
            let marker = record["delete_marker"] == true;
            if recorded == "null"
                || !record["delete_marker"].is_boolean()
                || !record["started"].is_boolean()
                || !record["deleted"].is_boolean()
                || record["delete_marker"] != row["delete_marker"]
                || (record["deleted"] == true && record["started"] != true)
                || (marker && (version.as_deref() != Some(recorded) || !record["etag"].is_null()))
                || (!marker && etag(&record["etag"]).is_none())
            {
                return Err(message(
                    "archive cancellation contains an invalid private copied version",
                ));
            }
            let object = key(store, text(row, "destination_object")?);
            let id = (object.clone(), recorded.to_owned());
            if !tracked.insert(id.clone()) {
                return Err(message(
                    "archive cancellation repeated a private copied version",
                ));
            }
            let item = actual_by_id.get(&id).map(|index| &actual[*index]);
            let Some(item) = item else {
                if record["started"] != true {
                    return Err(message(
                        "archive cancellation lost an unretired copied version",
                    ));
                }
                continue;
            };
            if item["delete_marker"] != row["delete_marker"] {
                return Err(message(
                    "archive cancellation found a mismatched copied delete marker",
                ));
            }
            if !marker {
                let info = cached_version_head(store, &object, recorded, &mut heads)?;
                validate_payload_head(
                    info,
                    text(row, "destination_object")?,
                    recorded,
                    &row["size"],
                    &record["etag"],
                )?;
                if !metadata_owned(info, &metadata, &legacy_metadata, &row_token) {
                    return Err(message(
                        "archive cancellation cannot verify copied version ownership",
                    ));
                }
                if item["Size"] != row["size"] || etag(&item["ETag"]) != etag(&record["etag"]) {
                    return Err(message(
                        "archive cancellation found mismatched copied payload metadata",
                    ));
                }
            }
            owned_ids.insert(id);
            owned.push(OwnedVersion {
                row: index,
                record: record_index,
                key: object,
                version: recorded.to_owned(),
                marker,
            });
        }
    }
    for index in 0..rows(journal)?.len() {
        let row = journal["versions"][index].clone();
        let object = key(store, text(&row, "destination_object")?);
        let row_token = token(journal, &row)?;
        for item_index in actual_by_key.get(&object).into_iter().flatten() {
            let item = &actual[*item_index];
            let id = identity(item)?;
            if owned_ids.contains(&id) {
                continue;
            }
            if row["delete_marker"] == true {
                if row["started"] == true
                    && row["destination_version_id"].is_null()
                    && item["delete_marker"] == true
                {
                    return Err(message(
                        "archive cancellation cannot prove ownership of an unrecorded delete marker",
                    ));
                }
                continue;
            }
            if item["delete_marker"] == true {
                continue;
            }
            let info = cached_version_head(store, &id.0, &id.1, &mut heads)?;
            if !metadata_owned(info, &metadata, &legacy_metadata, &row_token) {
                continue;
            }
            if item["VersionId"] == "null"
                || info["DeleteMarker"] == true
                || info["VersionId"] != item["VersionId"]
                || info["ContentLength"] != row["size"]
                || item["Size"] != row["size"]
                || etag(&item["ETag"]).is_none()
                || etag(&info["ETag"]) != etag(&item["ETag"])
            {
                return Err(message(
                    "archive cancellation recovered a mismatched copied version",
                ));
            }
            let records = journal["versions"][index]["cancel_owned_versions"]
                .as_array_mut()
                .ok_or_else(|| message("invalid cancel inventory"))?;
            let record = records.len();
            records.push(json!({"version_id":item["VersionId"],"etag":etag(&item["ETag"]),"delete_marker":false,"started":false,"deleted":false}));
            tracked.insert(id.clone());
            owned_ids.insert(id.clone());
            owned.push(OwnedVersion {
                row: index,
                record,
                key: id.0,
                version: id.1,
                marker: false,
            });
        }
    }
    let unrelated = actual
        .into_iter()
        .filter(|item| identity(item).is_ok_and(|id| !owned_ids.contains(&id)))
        .collect();
    if verify_originals {
        verify_cancel_source_with(store, context, journal)?;
    }
    Ok((owned, unrelated))
}
fn version_report(items: &[Value]) -> Value {
    items.iter().map(|item| json!({"key":item["Key"],"version_id":item["VersionId"],"delete_marker":item["delete_marker"]})).collect::<Vec<_>>().into()
}
fn cancel_copy(
    store: &impl Storage,
    context: &Value,
    journal: &mut Value,
    state: &Path,
    preview: bool,
) -> Result<Value> {
    let terminal = journal["status"] == "cancelled";
    let (owned, unrelated) = cancel_inventory(store, context, journal, !terminal)?;
    let uploads = rows(journal)?.iter().filter(|row| row["multipart_upload_id"].is_string()).map(|row| json!({"object":row["destination_object"],"upload_id":row["multipart_upload_id"]})).collect::<Vec<_>>();
    let owned_uploads = uploads
        .iter()
        .map(|upload| {
            Ok((
                key(store, text(upload, "object")?),
                text(upload, "upload_id")?.to_owned(),
            ))
        })
        .collect::<Result<BTreeSet<_>>>()?;
    let inventory = destination_uploads(store, context)?;
    let unrelated_uploads = inventory
        .iter()
        .filter(|item| {
            !owned_uploads.contains(&(
                item["key"].as_str().unwrap_or("").to_owned(),
                item["upload_id"].as_str().unwrap_or("").to_owned(),
            ))
        })
        .cloned()
        .collect::<Vec<_>>();
    if terminal {
        if !owned.is_empty()
            || inventory.iter().any(|item| {
                owned_uploads.contains(&(
                    item["key"].as_str().unwrap_or("").to_owned(),
                    item["upload_id"].as_str().unwrap_or("").to_owned(),
                ))
            })
        {
            return Err(message(
                "completed archive cancellation unexpectedly contains owned versions or uploads",
            ));
        }
        if !preview
            && journal["schema_version"] != crate::policy::ARCHIVE_COPY_JOURNAL_SCHEMA_VERSION
        {
            save_journal(state, journal)?;
        }
        return Ok(
            json!({"status":"cancelled","already_cancelled":true,"deleted_versions":[],"delete_versions":[],"retained_versions":version_report(&unrelated),"uploads":[],"retained_uploads":unrelated_uploads}),
        );
    }
    let deletions = owned
        .iter()
        .map(
            |entry| json!({"Key":entry.key,"VersionId":entry.version,"delete_marker":entry.marker}),
        )
        .collect::<Vec<_>>();
    if preview {
        return Ok(
            json!({"status":"would_cancel","delete_versions":version_report(&deletions),"retained_versions":version_report(&unrelated),"uploads":uploads,"retained_uploads":unrelated_uploads}),
        );
    }
    journal["status"] = "canceling".into();
    save_journal(state, journal)?;
    for index in 0..rows(journal)?.len() {
        abort_upload(store, &mut journal["versions"][index], || Ok(()))?;
        save_journal(state, journal)?;
    }
    for entry in &owned {
        journal["versions"][entry.row]["cancel_owned_versions"][entry.record]["started"] =
            true.into();
        if journal["versions"][entry.row]["destination_version_id"] == entry.version {
            journal["versions"][entry.row]["cancel_started"] = true.into();
        }
        save_journal(state, journal)?;
        store.call(
            "delete_object",
            &json!({"Bucket":store.bucket(),"Key":entry.key,"VersionId":entry.version}),
            None,
        )?;
        journal["versions"][entry.row]["cancel_owned_versions"][entry.record]["deleted"] =
            true.into();
        if journal["versions"][entry.row]["destination_version_id"] == entry.version {
            journal["versions"][entry.row]["cancel_deleted"] = true.into();
        }
        save_journal(state, journal)?;
    }
    for row in journal["versions"]
        .as_array_mut()
        .ok_or_else(|| message("invalid cancel inventory"))?
    {
        for record in row["cancel_owned_versions"]
            .as_array_mut()
            .ok_or_else(|| message("invalid cancel inventory"))?
        {
            if record["started"] == true {
                record["deleted"] = true.into();
            }
        }
        if row["cancel_started"] == true {
            row["cancel_deleted"] = true.into();
        }
    }
    let (remaining_owned, remaining) = cancel_inventory(store, context, journal, false)?;
    let remaining_uploads = destination_uploads(store, context)?;
    if !remaining_owned.is_empty() {
        save_journal(state, journal)?;
        return Err(message(
            "archive cancellation still contains an owned copied version",
        ));
    }
    if remaining_uploads.iter().any(|item| {
        owned_uploads.contains(&(
            item["key"].as_str().unwrap_or("").to_owned(),
            item["upload_id"].as_str().unwrap_or("").to_owned(),
        ))
    }) {
        return Err(message(
            "archive cancellation still contains an owned multipart upload",
        ));
    }
    let complete = remaining.is_empty() && remaining_uploads.is_empty();
    journal["status"] = if complete { "cancelled" } else { "canceling" }.into();
    save_journal(state, journal)?;
    Ok(
        json!({"status":if complete {"cancelled"} else {"cancelled_with_unrelated_history"},"deleted_versions":version_report(&deletions),"retained_versions":version_report(&remaining),"uploads":uploads,"retained_uploads":remaining_uploads}),
    )
}

pub(crate) fn execute(repo: &GitRepo, operation: &str, payload: &Value) -> Result<Value> {
    require_archive_protocol()?;
    let client = S3Client::from_repo(repo)?;
    require_versioning(&client)?;
    execute_with(&client, repo, operation, payload)
}
fn execute_with(
    store: &impl Storage,
    repo: &GitRepo,
    operation: &str,
    payload: &Value,
) -> Result<Value> {
    let context = context(store, payload)?;
    if operation == "plan" {
        let versions = source_inventory(store, &context)?;
        if !destination_inventory(store, &context)?.is_empty()
            || !destination_uploads(store, &context)?.is_empty()
        {
            return Err(message(
                "archive destination already contains object history or multipart uploads",
            ));
        }
        let mut receipt = context;
        receipt["status"] = "planned".into();
        receipt["versions"] = versions.into();
        receipt["source_cleanup"] = "after_verified_git_publication".into();
        return Ok(receipt);
    }
    if ![
        "copy",
        "verify",
        "verify-source",
        "cancel",
        "cancel-preview",
    ]
    .contains(&operation)
    {
        return Err(message(format!(
            "unknown archive storage operation: {operation:?}"
        )));
    }
    let state = payload["state_path"].as_str().map(Path::new);
    let mut journal = if let Some(state) = state {
        load_copy_journal(state)?
    } else {
        None
    };
    if ["cancel", "cancel-preview"].contains(&operation) {
        let Some(mut journal) = journal else {
            return Ok(json!({"status":"no_remote_copy","retained_versions":[],"uploads":[]}));
        };
        validate_receipt(&journal, &context)?;
        return cancel_copy(
            store,
            &context,
            &mut journal,
            state.ok_or_else(|| message("archive cancel requires a private journal"))?,
            operation == "cancel-preview",
        );
    }
    if ["verify", "verify-source"].contains(&operation) {
        let migration_receipt = payload
            .get("receipt")
            .filter(|receipt| receipt.get("migration_kind").is_some())
            .cloned();
        let mut receipt = migration_receipt
            .or(journal)
            .or_else(|| payload.get("receipt").cloned())
            .or_else(|| payload.get("planned").cloned())
            .or_else(|| payload.get("versions").is_some().then(|| payload.clone()))
            .ok_or_else(|| message("archive verification requires a receipt"))?;
        validate_receipt(&receipt, &context)?;
        validated_rename_receipt(&receipt)?;
        if operation == "verify-source" {
            if source_signature(&source_inventory(store, &context)?)
                != source_signature(rows(&receipt)?)
            {
                return Err(message(
                    "archive receipt does not preserve the complete live source history",
                ));
            }
            return public_receipt(&receipt, Some("source-verified"));
        }
        verify_receipt_history_with(store, &context, &mut receipt)?;
        return public_receipt(&receipt, Some("verified"));
    }
    let state =
        state.ok_or_else(|| message("archive copy requires a durable private journal path"))?;
    verify_reservation(
        repo,
        &context,
        payload,
        journal
            .as_ref()
            .filter(|journal| journal["status"] != "cancelled"),
    )?;
    if let Some(current) = journal.as_ref() {
        validate_receipt(current, &context)?;
        if current["status"] == "canceling" {
            return Err(message(
                "archive cancellation is incomplete; finish cancel before starting another copy",
            ));
        }
        if current["status"] == "cancelled" {
            if !destination_inventory(store, &context)?.is_empty()
                || !destination_uploads(store, &context)?.is_empty()
            {
                return Err(message(
                    "archive destination contains unrelated history after cancellation",
                ));
            }
            journal = None;
        } else if current["schema_version"] != crate::policy::ARCHIVE_COPY_JOURNAL_SCHEMA_VERSION {
            // Even an already-copied retry must fence 0.6.0's resume path;
            // validation and ownership checks precede this durable upgrade.
            save_journal(state, current)?;
        }
    }
    if let Some(current) = journal
        .as_mut()
        .filter(|journal| journal["status"] == "copied")
    {
        if let Some(planned) = payload.get("planned") {
            validate_receipt(planned, &context)?;
            if source_signature(rows(planned)?) != source_signature(rows(current)?) {
                return Err(message(
                    "retained archive copy differs from this attempt's complete source history",
                ));
            }
        }
        verify_history_with(store, &context, current)?;
        verify_reservation(repo, &context, payload, Some(current))?;
        return public_receipt(current, None);
    }
    let current = source_inventory(store, &context)?;
    if journal.is_none() {
        if let Some(planned) = payload.get("planned") {
            validate_receipt(planned, &context)?;
            if source_signature(rows(planned)?) != source_signature(&current) {
                return Err(message("archive source history changed after planning"));
            }
        }
        if !destination_inventory(store, &context)?.is_empty()
            || !destination_uploads(store, &context)?.is_empty()
        {
            return Err(message(
                "archive destination already contains object history or multipart uploads",
            ));
        }
        let parent = state
            .parent()
            .ok_or_else(|| message("archive journal has no parent"))?;
        fs::create_dir_all(parent).at(parent)?;
        let nonce = tempfile::Builder::new()
            .prefix("copy-")
            .rand_bytes(32)
            .tempfile_in(parent)
            .at(parent)?;
        let nonce = nonce
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| message("archive transaction is not UTF-8"))?;
        // S3 normalizes metadata names to lowercase. Hash the random nonce so
        // the persisted token key round-trips through HTTP exactly.
        let transaction = digest(nonce.as_bytes());
        let mut receipt = context.clone();
        receipt["schema_version"] = crate::policy::ARCHIVE_COPY_JOURNAL_SCHEMA_VERSION.into();
        receipt["status"] = "copying".into();
        receipt["transaction_id"] = transaction.into();
        receipt["versions"] = current.clone().into();
        save_journal(state, &receipt)?;
        journal = Some(receipt);
    }
    let mut journal = journal.ok_or_else(|| message("archive copy has no private journal"))?;
    validate_receipt(&journal, &context)?;
    text(&journal, "transaction_id")?;
    if source_signature(rows(&journal)?) != source_signature(&current) {
        return Err(message("archive source history changed during copying"));
    }
    validate_destination(store, &context, &mut journal, true, false)?;
    save_journal(state, &journal)?;
    let mut written = rows(&journal)?
        .iter()
        .filter(|row| row["destination_version_id"].is_string())
        .map(|row| text(row, "destination_object").map(str::to_owned))
        .collect::<Result<BTreeSet<_>>>()?;
    for index in 0..rows(&journal)?.len() {
        if journal["versions"][index]["destination_version_id"].is_string() {
            continue;
        }
        let destination = text(&journal["versions"][index], "destination_object")?.to_owned();
        if store.b2() && written.contains(&destination) {
            thread::sleep(Duration::from_millis(1050));
        }
        let mut row = journal["versions"][index].clone();
        abort_upload(store, &mut row, || {
            verify_reservation(repo, &context, payload, Some(&journal))
        })?;
        journal["versions"][index] = row;
        journal["versions"][index]["started"] = true.into();
        save_journal(state, &journal)?;
        if journal["versions"][index]["delete_marker"] == true {
            verify_reservation(repo, &context, payload, Some(&journal))?;
            let response = store
                .call(
                    "delete_object",
                    &json!({"Bucket":store.bucket(),"Key":key(store,&destination)}),
                    None,
                )?
                .value;
            if response["DeleteMarker"] != true {
                return Err(message(
                    "archive delete-marker copy did not return a delete marker",
                ));
            }
            record_destination(
                &mut journal["versions"][index],
                &response["VersionId"],
                &Value::Null,
                &Value::Null,
            )?;
        } else {
            copy_payload(store, repo, &context, payload, &mut journal, index, state)?;
        }
        written.insert(destination);
        save_journal(state, &journal)?;
    }
    if source_signature(&source_inventory(store, &context)?) != source_signature(rows(&journal)?) {
        return Err(message("archive source history changed before completion"));
    }
    verify_history_with(store, &context, &mut journal)?;
    verify_reservation(repo, &context, payload, Some(&journal))?;
    journal["status"] = "copied".into();
    save_journal(state, &journal)?;
    public_receipt(&journal, None)
}

fn registry_key(store: &impl Storage, source: &str) -> Result<String> {
    relative_path(source)?;
    Ok(key(
        store,
        &format!(".workspace-mgr/archive/{}.json", digest(source.as_bytes())),
    ))
}
fn validate_registry_receipt(store: &impl Storage, receipt: &Value) -> Result<()> {
    if !receipt.is_object() || receipt["schema_version"] != 1 {
        return Err(message("unsupported archive registry receipt schema"));
    }
    validated_rename_receipt(receipt)?;
    let source = relative_path(text(receipt, "source")?)?;
    let destination = relative_path(text(receipt, "destination")?)?;
    if source == destination {
        return Err(message(
            "archive registry source and destination are identical",
        ));
    }
    if receipt["bucket"] != store.bucket()
        || receipt["remote_prefix"] != store.prefix()
        || receipt["remote"] != "workspace-mgr"
    {
        return Err(message(
            "archive registry receipt selects another storage location or remote",
        ));
    }
    let mut identities = BTreeMap::new();
    for row in rows(receipt)? {
        let old = text(row, "source_object")?;
        let new = text(row, "destination_object")?;
        if old.starts_with('/')
            || new.starts_with('/')
            || !old.starts_with(&format!("{source}/"))
            || new != format!("{destination}{}", &old[source.len()..])
        {
            return Err(message(
                "archive registry object does not match its source and destination",
            ));
        }
        let source_version = text(row, "source_version_id")?;
        let destination_version = text(row, "destination_version_id")?;
        if destination_version == "null" || !row["delete_marker"].is_boolean() {
            return Err(message(
                "archive registry mapping has no exact destination version or marker type",
            ));
        }
        if row["delete_marker"] != true
            && (row["size"].as_u64().is_none() || etag(&row["destination_etag"]).is_none())
        {
            return Err(message(
                "archive registry data mapping has no valid size or destination etag",
            ));
        }
        if identities
            .insert((old, source_version), row)
            .is_some_and(|previous| previous != row)
        {
            return Err(message(
                "archive registry contains conflicting version mappings",
            ));
        }
    }
    Ok(())
}
fn verify_coordination_with(
    store: &impl Storage,
    repo: &GitRepo,
    receipt: &Value,
    proof: &Value,
    allow_published: bool,
) -> Result<()> {
    validate_registry_receipt(store, receipt)?;
    if proof["mode"] != "git-cas" {
        return Err(message(
            "archive registry mutation requires a verified Git CAS binding",
        ));
    }
    let body = canonical(receipt)?;
    if proof["receipt_sha256"] != digest(&body)
        || proof["ref"] != crate::archive_registry::binding_ref(receipt)?
    {
        return Err(message(
            "archive registry Git binding selects another receipt or storage identity",
        ));
    }
    let oid = text(proof, "oid")?;
    let remote = text(proof, "remote")?;
    let configured = Config::load(repo)?;
    if !valid_oid(oid)
        || remote.starts_with('-')
        || remote.chars().any(char::is_control)
        || configured.git.remote != remote
    {
        return Err(message(
            "archive registry Git binding has no valid configured remote or object identity",
        ));
    }
    let publication = proof["publication_oid"]
        .as_str()
        .filter(|value| !value.is_empty());
    if let Some(revision) = publication {
        if !allow_published {
            return Err(message(
                "published archive evidence cannot withdraw a mapping",
            ));
        }
        if !valid_oid(revision) {
            return Err(message(
                "archive registry has no exact Git publication identity",
            ));
        }
        let path = relative_path(text(proof, "receipt_path")?)?;
        if path
            != format!(
                "{}/{}",
                text(receipt, "destination")?,
                crate::archive_migration::RECEIPT_NAME
            )
        {
            return Err(message(
                "archive registry publication proof does not select its canonical archived receipt path",
            ));
        }
    } else {
        let journal =
            load_copy_journal(Path::new(text(proof, "state_path")?))?.ok_or_else(|| {
                message("archive registry mutation requires its private copy journal")
            })?;
        let transaction = text(proof, "transaction_id")?;
        if journal["transaction_id"] != transaction || receipt["transaction_id"] != transaction {
            return Err(message(
                "archive registry mutation is not owned by this private copy transaction",
            ));
        }
        let mut public = public_receipt(&journal, None)?;
        public["status"] = receipt["status"].clone();
        let mut expected = receipt.clone();
        for value in [&mut public, &mut expected] {
            for name in crate::archive_migration::RECEIPT_METADATA_FIELDS {
                object_mut(value)?.remove(name);
            }
        }
        if canonical(&public)? != canonical(&expected)? {
            return Err(message(
                "archive registry receipt differs from its private copy journal",
            ));
        }
    }
    let reference = text(proof, "ref")?;
    let fetch = git(repo, &["remote", "get-url", "--all", remote])?;
    let push = git(repo, &["remote", "get-url", "--push", "--all", remote])?;
    if fetch
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .count()
        != 1
        || fetch != push
    {
        return Err(message(
            "archive coordination requires one identical verified Git fetch and push destination",
        ));
    }
    if git(repo, &["ls-remote", "--refs", "--", remote, reference])?
        != format!("{oid}\t{reference}\n").as_bytes()
    {
        return Err(message(
            "archive registry Git CAS binding was removed or replaced",
        ));
    }
    if crate::archive_git_control::read_body(&repo.root, oid)?.as_bytes() != body {
        return Err(message(
            "archive registry Git CAS control body differs from its receipt",
        ));
    }
    if let Some(revision) = publication {
        let branch = format!("refs/heads/{}", configured.git.branch);
        if proof
            .get("base_branch")
            .is_some_and(|value| value != configured.git.branch.as_str())
            || git(repo, &["ls-remote", "--refs", "--", remote, &branch])?
                != format!("{revision}\t{branch}\n").as_bytes()
        {
            return Err(message(
                "archive registry publication proof is not the current configured shared branch",
            ));
        }
        let object = format!("{revision}:{}", text(proof, "receipt_path")?);
        let published: Value = serde_json::from_slice(&git(repo, &["show", &object])?)
            .map_err(|_| message("archive registry Git publication receipt is invalid"))?;
        if canonical(&published)? != body {
            return Err(message(
                "archive registry Git publication selects another receipt",
            ));
        }
    }
    Ok(())
}
pub(crate) fn verify_coordination(
    client: &S3Client,
    repo: &GitRepo,
    receipt: &Value,
    proof: &Value,
    allow_published: bool,
) -> Result<()> {
    verify_coordination_with(client, repo, receipt, proof, allow_published)
}
fn registry_versions(store: &impl Storage, object: &str) -> Result<BTreeSet<String>> {
    let mut versions = BTreeSet::new();
    for item in list_versions(store, object)? {
        if item["Key"] != object {
            continue;
        }
        if item["delete_marker"] == true {
            return Err(message(
                "archive registry history contains a delete marker; refusing a hidden mapping",
            ));
        }
        let version = text(&item, "VersionId")?;
        if version == "null" {
            return Err(message(
                "archive registry requires exact non-null object versions",
            ));
        }
        versions.insert(version.to_owned());
    }
    Ok(versions)
}
fn registry_read_version(
    store: &impl Storage,
    source: &str,
    object: &str,
    version: &str,
) -> Result<Value> {
    let receipt =
        store.read_registry(&json!({"Bucket":store.bucket(),"Key":object,"VersionId":version}))?;
    validate_registry_source(store, source, &receipt)?;
    Ok(receipt)
}
fn validate_registry_source(store: &impl Storage, source: &str, receipt: &Value) -> Result<()> {
    validate_registry_receipt(store, receipt)?;
    if receipt["source"] != source {
        return Err(message(
            "archive registry source does not match its canonical key",
        ));
    }
    Ok(())
}
fn registry_read_with(store: &impl Storage, source: &str) -> Result<Option<Value>> {
    registry_read_bounded(store, source, READ_WORKERS)
}
fn registry_read_bounded(
    store: &impl Storage,
    source: &str,
    limit: usize,
) -> Result<Option<Value>> {
    let limit = limit.clamp(1, READ_WORKERS);
    let object = registry_key(store, source)?;
    for _ in 0..3 {
        let versions = registry_versions(store, &object)?;
        let mut selected = None;
        let mut encoded = None;
        let version_ids = versions.iter().collect::<Vec<_>>();
        // Bound both concurrent downloads and decoded receipt memory. Keep the
        // complete before/after version sets so parallel reads cannot hide a
        // registry generation added or removed during this validation pass.
        for chunk in version_ids.chunks(limit) {
            let requests = chunk
                .iter()
                .map(|version| {
                    json!({"Bucket":store.bucket(),
                "Key":object,"VersionId":version})
                })
                .collect::<Vec<_>>();
            let receipts = store.read_registries(&requests, limit)?;
            if receipts.len() != requests.len() {
                return Err(message(
                    "archive registry batch returned an incomplete inventory",
                ));
            }
            for receipt in receipts {
                validate_registry_source(store, source, &receipt)?;
                let body = canonical(&receipt)?;
                if encoded.as_ref().is_some_and(|previous| previous != &body) {
                    return Err(message(format!(
                        "conflicting archive registry history at {object:?}"
                    )));
                }
                selected = Some(receipt);
                encoded = Some(body);
            }
        }
        if registry_versions(store, &object)? == versions {
            return Ok(selected);
        }
    }
    Err(message(
        "archive registry history changed while verifying its complete versions",
    ))
}
pub(crate) fn registry_read(
    client: &S3Client,
    repo: &GitRepo,
    source: &str,
) -> Result<Option<Value>> {
    registry_read_with_parallelism(client, repo, source, READ_WORKERS)
}
pub(crate) fn registry_read_with_parallelism(
    client: &S3Client,
    _repo: &GitRepo,
    source: &str,
    limit: usize,
) -> Result<Option<Value>> {
    registry_read_bounded(client, source, limit)
}
fn registry_lookup_with(
    store: &impl Storage,
    object: &str,
    version: &str,
) -> Result<Option<Value>> {
    let prefix = if store.prefix().is_empty() {
        String::new()
    } else {
        format!("{}/", store.prefix())
    };
    let relative = object
        .strip_prefix(&prefix)
        .ok_or_else(|| message("historical archive lookup escaped its storage prefix"))?;
    if relative.is_empty() || relative.starts_with('/') {
        return Err(message("invalid archive registry lookup object"));
    }
    let parts = relative.split('/').collect::<Vec<_>>();
    for length in (1..parts.len()).rev() {
        let source = parts[..length].join("/");
        if relative_path(&source).is_err() {
            continue;
        }
        let Some(receipt) = registry_read_with(store, &source)? else {
            continue;
        };
        for row in rows(&receipt)? {
            if row["source_object"] != relative || row["source_version_id"] != version {
                continue;
            }
            if row["delete_marker"] == true {
                return Err(message("historical data lookup selected a delete marker"));
            }
            let mut mapping = row.clone();
            mapping["destination_key"] = key(store, text(row, "destination_object")?).into();
            return Ok(Some(mapping));
        }
    }
    Ok(None)
}
pub(crate) fn registry_lookup(
    client: &S3Client,
    _repo: &GitRepo,
    object: &str,
    version: &str,
) -> Result<Option<Value>> {
    registry_lookup_with(client, object, version)
}
fn authorize_publish(
    store: &impl Storage,
    repo: &GitRepo,
    receipt: &Value,
    proof: &Value,
) -> Result<()> {
    verify_coordination_with(store, repo, receipt, proof, true)?;
    if proof["publication_oid"].as_str().is_none_or(str::is_empty) {
        let journal = load_copy_journal(Path::new(text(proof, "state_path")?))?
            .ok_or_else(|| message("archive publication has no copy journal"))?;
        if journal["status"] != "copied"
            || rows(&journal)?.iter().any(|row| {
                row["cancel_started"] == true
                    || row["cancel_deleted"] == true
                    || row["cancel_owned_versions"]
                        .as_array()
                        .is_some_and(|rows| !rows.is_empty())
            })
        {
            return Err(message(
                "archive registry publication requires a complete uncancelled copy journal",
            ));
        }
    }
    Ok(())
}
fn registry_matches(store: &impl Storage, receipt: &Value) -> Result<bool> {
    let Some(existing) = registry_read_with(store, text(receipt, "source")?)? else {
        return Ok(false);
    };
    if canonical(&existing)? != canonical(receipt)? {
        return Err(message("conflicting archive registry history"));
    }
    Ok(true)
}
fn registry_publish(
    store: &impl Storage,
    repo: &GitRepo,
    receipt: &Value,
    proof: &Value,
) -> Result<Value> {
    validate_registry_receipt(store, receipt)?;
    authorize_publish(store, repo, receipt, proof)?;
    fence_owner_journal(proof)?;
    let object = registry_key(store, text(receipt, "source")?)?;
    if registry_matches(store, receipt)? {
        authorize_publish(store, repo, receipt, proof)?;
        return Ok(json!({"status":"unchanged","registry_key":object}));
    }
    let body = canonical(receipt)?;
    let mut request = json!({"Bucket":store.bucket(),"Key":object,"ContentType":"application/json",
        "ContentMD5":base64::engine::general_purpose::STANDARD.encode(Md5::digest(&body)),"IfNoneMatch":"*"});
    authorize_publish(store, repo, receipt, proof)?;
    let first = store.call("put_object", &request, Some(&body));
    if let Err(error) = first {
        if registry_matches(store, receipt)? {
            authorize_publish(store, repo, receipt, proof)?;
            return Ok(json!({"status":"unchanged","registry_key":object}));
        }
        if error.code == "NotImplemented" || error.code == "NotSupported" {
            if store.b2() {
                if error
                    .header
                    .as_deref()
                    .is_some_and(|header| !header.eq_ignore_ascii_case("if-none-match"))
                {
                    return Err(message(format!(
                        "archive registry provider rejected request header {:?}",
                        error.header
                    )));
                }
                // The claim is held through both copies and registry cleanup.
                // It replaces unsupported provider CAS, never receipt identity.
                authorize_publish(store, repo, receipt, proof)?;
                object_mut(&mut request)?.remove("IfNoneMatch");
                if let Err(error) = store.call("put_object", &request, Some(&body)) {
                    if registry_matches(store, receipt)? {
                        authorize_publish(store, repo, receipt, proof)?;
                        return Ok(json!({"status":"unchanged","registry_key":object}));
                    }
                    return Err(error.into());
                }
            } else {
                return Err(message(
                    "archive registry provider rejected a request header; atomic conditional publication is unavailable, refusing an unsafe unconditional write",
                ));
            }
        } else {
            return Err(error.into());
        }
    }
    if !registry_matches(store, receipt)? {
        return Err(message("archive registry publication was not readable"));
    }
    authorize_publish(store, repo, receipt, proof)?;
    Ok(json!({"status":"published","registry_key":object}))
}
fn registry_cancel(
    store: &impl Storage,
    repo: &GitRepo,
    receipt: &Value,
    proof: Option<&Value>,
    preview: bool,
) -> Result<Value> {
    let source = relative_path(text(receipt, "source")?)?;
    let object = registry_key(store, source)?;
    let Some(proof) = proof.filter(|value| !value.is_null()) else {
        if registry_read_with(store, source)?.is_some() {
            return Err(message(
                "archive registry cancellation requires the owning complete copy transaction",
            ));
        }
        return Ok(
            json!({"status":"no_registry","registry_key":object,"delete_registry_versions":[],"deleted_registry_versions":[]}),
        );
    };
    verify_coordination_with(store, repo, receipt, proof, false)?;
    verify_cancel_source_with(store, &receipt_context(receipt), receipt)?;
    let versions = registry_versions(store, &object)?;
    let body = canonical(receipt)?;
    for version in &versions {
        if canonical(&registry_read_version(store, source, &object, version)?)? != body {
            return Err(message(
                "archive registry cancellation found another transaction's receipt",
            ));
        }
    }
    if registry_versions(store, &object)? != versions {
        return Err(message(
            "archive registry changed during cancellation preview",
        ));
    }
    if preview {
        return Ok(
            json!({"status":"would_cancel","registry_key":object,"delete_registry_versions":versions}),
        );
    }
    fence_owner_journal(proof)?;
    let mut deleted = Vec::new();
    let mut absent = Vec::new();
    for version in &versions {
        verify_coordination_with(store, repo, receipt, proof, false)?;
        if registry_read_with(store, source)?
            .as_ref()
            .is_some_and(|current| canonical(current).is_ok_and(|current| current != body))
        {
            return Err(message("archive registry changed during cancellation"));
        }
        match store.call(
            "delete_object",
            &json!({"Bucket":store.bucket(),"Key":object,"VersionId":version}),
            None,
        ) {
            Ok(_) => deleted.push(version.clone()),
            Err(error) => {
                if registry_versions(store, &object)?.contains(version) {
                    return Err(error.into());
                }
                absent.push(version.clone());
            }
        }
    }
    verify_coordination_with(store, repo, receipt, proof, false)?;
    if !registry_versions(store, &object)?.is_empty() {
        return Err(message(
            "archive registry cancellation left object versions behind",
        ));
    }
    Ok(
        json!({"status":"cancelled","registry_key":object,"deleted_registry_versions":deleted,"already_absent":absent}),
    )
}
pub(crate) fn registry(repo: &GitRepo, operation: &str, payload: &Value) -> Result<Value> {
    require_archive_protocol()?;
    let client = S3Client::from_repo(repo)?;
    require_versioning(&client)?;
    registry_with(&client, repo, operation, payload)
}
fn require_archive_protocol() -> Result<()> {
    let installed = crate::config::installed_cli_version();
    let required = crate::policy::ARCHIVE_STORAGE_PROTOCOL_MINIMUM_CLI_VERSION;
    if !crate::config::cli_version_satisfies(&installed, &required) {
        return Err(message(format!(
            "archive storage coordination requires workspace-mgr {required} or newer; this build is workspace-mgr {installed}"
        )));
    }
    Ok(())
}
// Owner evidence is checked first. Persist the incompatible private protocol
// before any registry write or withdrawal; published evidence has no journal.
fn fence_owner_journal(proof: &Value) -> Result<()> {
    if proof["publication_oid"]
        .as_str()
        .is_some_and(|value| !value.is_empty())
    {
        return Ok(());
    }
    let path = Path::new(text(proof, "state_path")?);
    let journal = load_copy_journal(path)?
        .ok_or_else(|| message("archive registry mutation requires its private copy journal"))?;
    if journal["schema_version"] != crate::policy::ARCHIVE_COPY_JOURNAL_SCHEMA_VERSION {
        save_journal(path, &journal)?;
    }
    Ok(())
}
fn require_versioning(store: &impl Storage) -> Result<()> {
    let versioning = store
        .call(
            "get_bucket_versioning",
            &json!({"Bucket":store.bucket()}),
            None,
        )?
        .value;
    if versioning["Status"] != "Enabled" {
        return Err(message(
            "archive storage requires an enabled versioned bucket",
        ));
    }
    Ok(())
}
fn registry_with(
    store: &impl Storage,
    repo: &GitRepo,
    operation: &str,
    payload: &Value,
) -> Result<Value> {
    match operation {
        "publish" => registry_publish(
            store,
            repo,
            payload.get("receipt").unwrap_or(payload),
            payload.get("coordination").unwrap_or(&Value::Null),
        ),
        "cancel" | "cancel-preview" => registry_cancel(
            store,
            repo,
            &payload["receipt"],
            payload.get("coordination"),
            operation == "cancel-preview",
        ),
        "inspect" => {
            let receipt = registry_read_with(store, text(payload, "source")?)?;
            Ok(json!({"status":if receipt.is_some() {"mapped"} else {"missing"},"receipt":receipt}))
        }
        "read" => {
            let mapping = registry_lookup_with(
                store,
                text(payload, "object")?,
                text(payload, "version_id")?,
            )?;
            Ok(json!({"status":if mapping.is_some() {"mapped"} else {"missing"},"mapping":mapping}))
        }
        _ => Err(message(format!(
            "unknown archive registry operation: {operation:?}"
        ))),
    }
}

#[cfg(test)]
#[path = "native_archive_tests.rs"]
mod tests;
