//! Small, synchronous S3 REST transport. No implicit retry is performed for a
//! mutation: archive callers recover lost responses using their durable token
//! and exact version journal before deciding whether another write is safe.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::{Engine, engine::general_purpose::STANDARD};
use md5::Md5;
use quick_xml::{Reader, events::Event};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use url::Url;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::git::GitRepo;
use crate::hex::encode_lower;
use crate::s3_transport::{ReadResumingConnector, ReadResumingReader};

const MAX_LIST_PAGES: usize = 100_000;
const XML_LIMIT: u64 = 64 * 1024 * 1024;
const MEMORY_GET_LIMIT: u64 = 64 * 1024 * 1024;
const INTERRUPTED_READ_RETRIES: usize = 2;

pub(crate) const CREDENTIALS_NAME: &str = ".workspace-mgr/local/credentials.toml";

/// Machine-local authentication settings. Repository location and endpoint
/// are deliberately absent: the tracked repository config owns routing.
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CredentialsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_key_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_access_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_process: Option<String>,
}

impl CredentialsConfig {
    pub(crate) fn from_legacy_settings(settings: &BTreeMap<String, String>) -> Result<Self> {
        let value = Self {
            access_key_id: settings.get("access_key_id").cloned(),
            secret_access_key: settings.get("secret_access_key").cloned(),
            session_token: settings.get("session_token").cloned(),
            profile: settings.get("profile").cloned(),
            region: settings.get("region").cloned(),
            credential_process: settings.get("credential_process").cloned(),
        };
        value.validate()?;
        Ok(value)
    }

    pub(crate) fn render(&self) -> Result<String> {
        self.validate()?;
        toml::to_string_pretty(self)
            .map_err(|_| Error::message("cannot render local S3 credentials configuration"))
    }

    fn validate(&self) -> Result<()> {
        for (field, value) in [
            ("access_key_id", &self.access_key_id),
            ("secret_access_key", &self.secret_access_key),
            ("session_token", &self.session_token),
            ("profile", &self.profile),
            ("region", &self.region),
            ("credential_process", &self.credential_process),
        ] {
            if value
                .as_ref()
                .is_some_and(|value| value.trim().is_empty() || value.contains(['\n', '\r']))
            {
                return Err(Error::message(format!(
                    "local S3 credentials field {field} must be a non-empty single-line string"
                )));
            }
        }
        if self.access_key_id.is_some() != self.secret_access_key.is_some() {
            return Err(Error::message(
                "local S3 credentials are incomplete: access_key_id and secret_access_key must be configured together",
            ));
        }
        if self.session_token.is_some() && self.access_key_id.is_none() {
            return Err(Error::message(
                "local S3 session_token requires access_key_id and secret_access_key",
            ));
        }
        Ok(())
    }

    fn settings(&self) -> BTreeMap<String, String> {
        [
            ("access_key_id", &self.access_key_id),
            ("secret_access_key", &self.secret_access_key),
            ("session_token", &self.session_token),
            ("profile", &self.profile),
            ("region", &self.region),
            ("credential_process", &self.credential_process),
        ]
        .into_iter()
        .filter_map(|(key, value)| value.clone().map(|value| (key.to_owned(), value)))
        .collect()
    }
}

pub(crate) fn credentials_path(repo: &GitRepo) -> Result<PathBuf> {
    let directory = crate::local_state::directory_unmigrated(repo)?;
    crate::path::reject_symlink_traversal(&directory, "credentials.toml", "local S3 credentials")?;
    Ok(directory.join("credentials.toml"))
}

pub(crate) fn load_credentials_config(repo: &GitRepo) -> Result<CredentialsConfig> {
    let path = credentials_path(repo)?;
    let raw = read_optional(&path)?;
    let config: CredentialsConfig = toml::from_str(&raw).map_err(|_| {
        Error::message(format!(
            "invalid local S3 credentials configuration: {CREDENTIALS_NAME}; use flat TOML string fields access_key_id, secret_access_key, session_token, profile, region or credential_process"
        ))
    })?;
    config.validate()?;
    Ok(config)
}

#[derive(Clone)]
struct Credentials {
    access: String,
    secret: String,
    token: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct S3Error {
    pub status: Option<u16>,
    pub code: String,
    pub message: String,
    pub header: Option<String>,
    pub details: Value,
}

impl S3Error {
    fn local(message: impl Into<String>) -> Self {
        Self {
            status: None,
            code: "InvalidRequest".to_owned(),
            message: message.into(),
            header: None,
            details: Value::Null,
        }
    }

    fn transport(message: impl Into<String>) -> Self {
        Self {
            status: None,
            code: "TransportError".to_owned(),
            message: message.into(),
            header: None,
            details: Value::Null,
        }
    }

    pub fn is_missing(&self) -> bool {
        matches!(
            self.code.as_str(),
            "NoSuchKey" | "NoSuchVersion" | "NotFound" | "404"
        ) || (self.code.is_empty() && self.status == Some(404))
    }

    pub fn is_retryable(&self) -> bool {
        self.code == "TransportError"
            || matches!(self.status, Some(408 | 429 | 500 | 502 | 503 | 504))
            || matches!(
                self.code.as_str(),
                "SlowDown" | "RequestTimeout" | "InternalError" | "ServiceUnavailable"
            )
    }
}

impl fmt::Display for S3Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "S3 {}", self.code)?;
        if let Some(status) = self.status {
            write!(formatter, " (HTTP {status})")?;
        }
        write!(formatter, ": {}", self.message)?;
        if let Some(request) = self.details["RequestId"].as_str() {
            write!(formatter, " (request {request})")?;
        }
        Ok(())
    }
}

impl std::error::Error for S3Error {}

impl From<S3Error> for Error {
    fn from(value: S3Error) -> Self {
        Self::message(value.to_string())
    }
}

#[derive(Debug)]
pub(crate) struct S3Response {
    pub value: Value,
    pub body: Vec<u8>,
}

#[derive(Clone)]
pub(crate) struct S3Client {
    pub bucket: String,
    pub prefix: String,
    pub b2: bool,
    endpoint: String,
    endpoint_path: String,
    host: String,
    region: String,
    credentials: Credentials,
    agent: ureq::Agent,
}

#[derive(Debug)]
struct RequestPlan {
    operation: String,
    method: &'static str,
    path: String,
    query: Vec<(String, String)>,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

type S3Result<T> = std::result::Result<T, S3Error>;

impl S3Client {
    pub(crate) fn cache_route(repo: &GitRepo) -> Result<Option<(String, Option<String>)>> {
        match fs::symlink_metadata(Config::path(repo)) {
            Ok(_) => Ok(Config::load_compatible(repo)?
                .s3
                .map(|s3| (s3.url, s3.endpoint_url))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(legacy_s3_configuration(&repo.root)?.map(|(url, endpoint, _)| (url, endpoint)))
            }
            Err(source) => Err(Error::Io {
                path: Config::path(repo),
                source,
            }),
        }
    }

    pub(crate) fn historical_cas_from_repo(repo: &GitRepo) -> Result<Option<Self>> {
        crate::path::reject_symlink_traversal(
            &repo.root,
            ".dvc/config",
            "legacy CAS configuration",
        )?;
        let raw = read_optional(&repo.root.join(".dvc/config"))?;
        if !crate::legacy_dvc::is_content_addressed_checkout(repo)? {
            return Ok(None);
        }
        let remote = ini_section(&raw, "core")["remote"].clone();
        let settings = legacy_remote_settings(&repo.root, &remote)?;
        let legacy_credentials = CredentialsConfig::from_legacy_settings(&settings)?;
        let (url, endpoint) = match fs::symlink_metadata(Config::path(repo)) {
            Ok(_) => {
                let s3 = Config::load_compatible(repo)?.s3.ok_or_else(|| {
                    Error::message("historical repository configuration does not enable S3")
                })?;
                (s3.url, s3.endpoint_url)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (
                settings.get("url").cloned().ok_or_else(|| {
                    Error::message("historical CAS remote has no public or legacy route")
                })?,
                settings.get("endpointurl").cloned(),
            ),
            Err(source) => {
                return Err(Error::Io {
                    path: Config::path(repo),
                    source,
                });
            }
        };
        let path = credentials_path(repo)?;
        let credentials = match fs::symlink_metadata(&path) {
            Ok(_) => load_credentials_config(repo)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => legacy_credentials,
            Err(source) => return Err(Error::Io { path, source }),
        };
        Self::from_location(&url, endpoint.as_deref(), &credentials).map(Some)
    }

    pub fn from_repo(repo: &GitRepo) -> Result<Self> {
        let (location, configured_endpoint, credentials_config) =
            match fs::symlink_metadata(Config::path(repo)) {
                Ok(_) => {
                    let config = Config::load_compatible(repo)?;
                    let s3 = config.s3.ok_or_else(|| {
                        Error::message("managed S3 is not configured in .workspace-mgr.toml")
                    })?;
                    (s3.url, s3.endpoint_url, load_credentials_config(repo)?)
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    // Historical checkouts predating native repository metadata
                    // retain their original remote definition for exact reads.
                    let (location, endpoint, legacy_credentials) =
                        legacy_s3_configuration(&repo.root)?.ok_or_else(|| {
                            Error::message("managed S3 has no repository configuration")
                        })?;
                    // Authentication is machine-local state. A migrated
                    // primary checkout owns it for all historical worktrees.
                    let path = credentials_path(repo)?;
                    let credentials = match fs::symlink_metadata(&path) {
                        Ok(_) => load_credentials_config(repo)?,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            legacy_credentials
                        }
                        Err(source) => return Err(Error::Io { path, source }),
                    };
                    (location, endpoint, credentials)
                }
                Err(source) => {
                    return Err(Error::Io {
                        path: Config::path(repo),
                        source,
                    });
                }
            };
        Self::from_location(
            &location,
            configured_endpoint.as_deref(),
            &credentials_config,
        )
    }

    pub(crate) fn from_location(
        location: &str,
        configured_endpoint: Option<&str>,
        credentials_config: &CredentialsConfig,
    ) -> Result<Self> {
        credentials_config.validate()?;
        let (bucket, prefix) = storage_location(location)?;
        let remote = credentials_config.settings();
        let profile = remote
            .get("profile")
            .cloned()
            .or_else(|| env_nonempty("AWS_PROFILE"))
            .or_else(|| env_nonempty("AWS_DEFAULT_PROFILE"))
            .unwrap_or_else(|| "default".to_owned());
        let (config, saved) = profile_settings(&profile)?;
        let endpoint = configured_endpoint.map(str::to_owned);
        let inferred_region = endpoint.as_deref().and_then(b2_region);
        let region = env_nonempty("AWS_REGION")
            .or_else(|| env_nonempty("AWS_DEFAULT_REGION"))
            .or_else(|| remote.get("region").cloned())
            .or_else(|| config.get("region").cloned())
            .or(inferred_region)
            .unwrap_or_else(|| "us-east-1".to_owned());
        if !region
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(Error::message("S3 region contains unsupported characters"));
        }
        let endpoint = endpoint.unwrap_or_else(|| {
            if region == "us-east-1" {
                "https://s3.amazonaws.com".to_owned()
            } else if region.starts_with("cn-") {
                format!("https://s3.{region}.amazonaws.com.cn")
            } else {
                format!("https://s3.{region}.amazonaws.com")
            }
        });
        let credentials = resolve_credentials(&remote, &saved, &config)?;
        Self::new(&bucket, &prefix, &endpoint, &region, credentials).map_err(Into::into)
    }

    fn new(
        bucket: &str,
        prefix: &str,
        endpoint: &str,
        region: &str,
        credentials: Credentials,
    ) -> S3Result<Self> {
        let parsed = Url::parse(endpoint).map_err(|_| S3Error::local("invalid S3 endpoint URL"))?;
        if !matches!(parsed.scheme(), "http" | "https")
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(S3Error::local(
                "S3 endpoint must be an HTTP(S) URL without credentials, query or fragment",
            ));
        }
        let hostname = parsed
            .host_str()
            .ok_or_else(|| S3Error::local("S3 endpoint has no host"))?;
        let b2 = hostname == "backblazeb2.com" || hostname.ends_with(".backblazeb2.com");
        let mut host = if hostname.contains(':') {
            format!("[{hostname}]")
        } else {
            hostname.to_owned()
        };
        if let Some(port) = parsed.port() {
            host.push_str(&format!(":{port}"));
        }
        let endpoint_origin = format!("{}://{host}", parsed.scheme());
        let mut agent_config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_global(Some(Duration::from_secs(1800)));
        // A local mock or explicitly local storage endpoint must never send
        // its requests through a user's global HTTP proxy.
        if hostname == "localhost"
            || hostname
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
        {
            agent_config = agent_config.proxy(None);
        }
        let agent = ureq::Agent::with_parts(
            agent_config.build(),
            ReadResumingConnector::new(ureq::unversioned::transport::DefaultConnector::default()),
            ureq::unversioned::resolver::DefaultResolver::default(),
        );
        Ok(Self {
            bucket: bucket.to_owned(),
            prefix: prefix.trim_end_matches('/').to_owned(),
            b2,
            endpoint: endpoint_origin,
            endpoint_path: parsed.path().trim_end_matches('/').to_owned(),
            host,
            region: region.to_owned(),
            credentials,
            agent,
        })
    }

    pub fn key_for(&self, object: &str) -> String {
        if self.prefix.is_empty() {
            object.to_owned()
        } else {
            format!("{}/{object}", self.prefix)
        }
    }

    pub fn call_s3(
        &self,
        operation: &str,
        args: &Value,
        body: Option<&[u8]>,
    ) -> S3Result<S3Response> {
        let plan = self.plan(operation, args, body)?;
        let response = self.run_buffered_request(&plan, &plan.body)?;
        self.read_response(operation, response)
    }

    /// Streams into a caller-owned scratch file. The caller verifies metadata,
    /// count and digest before atomically installing the cache object.
    pub fn get_to_file(&self, args: &Value, destination: &Path) -> S3Result<S3Response> {
        let plan = self.plan("get_object", args, None)?;
        let mut response = self.run_buffered_request(&plan, &[])?;
        if !response.status().is_success() {
            return self.read_response("get_object", response);
        }
        let value = response_metadata(response.headers())?;
        let expected = value["ContentLength"]
            .as_u64()
            .ok_or_else(|| S3Error::local("S3 GET returned no object Content-Length"))?;
        let mut output = File::create(destination)
            .map_err(|error| S3Error::local(format!("create download scratch file: {error}")))?;
        let count = std::io::copy(
            &mut response.body_mut().with_config().limit(u64::MAX).reader(),
            &mut output,
        )
        .map_err(|error| S3Error::transport(format!("incomplete S3 download: {error}")))?;
        if count != expected {
            return Err(S3Error::transport(
                "incomplete S3 download: Content-Length mismatch",
            ));
        }
        output
            .sync_all()
            .map_err(|error| S3Error::local(format!("sync downloaded object: {error}")))?;
        Ok(S3Response {
            value,
            body: Vec::new(),
        })
    }

    /// Hashes and rewinds one open file handle, then sends fixed-length bytes.
    /// There is no aws-chunked encoding or SDK checksum trailer on B2.
    pub fn put_file(&self, args: &Value, source: &Path) -> S3Result<S3Response> {
        let mut file = File::open(source)
            .map_err(|error| S3Error::local(format!("open upload cache file: {error}")))?;
        let (sha256, content_md5, length) = file_hashes(&mut file)?;
        if args["ExpectedSize"]
            .as_u64()
            .is_some_and(|expected| expected != length)
        {
            return Err(S3Error::local(
                "upload cache file size differs from its verified metadata",
            ));
        }
        if args["ExpectedMD5"].as_str().is_some_and(|expected| {
            STANDARD
                .decode(&content_md5)
                .is_ok_and(|bytes| encode_lower(bytes) != expected)
        }) {
            return Err(S3Error::local(
                "upload cache file content hash differs from its verified metadata",
            ));
        }
        if args["ContentMD5"]
            .as_str()
            .is_some_and(|expected| expected != content_md5)
        {
            return Err(S3Error::local(
                "upload cache file differs from its expected Content-MD5",
            ));
        }
        let mut plan = self.plan("put_object", args, None)?;
        plan.headers.insert("content-md5".to_owned(), content_md5);
        let request = self.request(
            &plan,
            ureq::SendBody::from_owned_reader(ReadResumingReader::new(file)),
            Some((&sha256, length)),
        )?;
        let response = self
            .agent
            .run(request)
            .map_err(|error| S3Error::transport(error.to_string()))?;
        self.read_response("put_object", response)
    }

    pub fn upload_part_file(
        &self,
        args: &Value,
        source: &Path,
        offset: u64,
        length: u64,
    ) -> S3Result<S3Response> {
        if length == 0 || length > 5 * 1024 * 1024 * 1024 {
            return Err(S3Error::local(
                "S3 multipart part must contain 1..5 GiB of bytes",
            ));
        }
        let mut file = File::open(source)
            .map_err(|error| S3Error::local(format!("open upload part cache file: {error}")))?;
        if offset
            .checked_add(length)
            .is_none_or(|end| file.metadata().map(|info| end > info.len()).unwrap_or(true))
        {
            return Err(S3Error::local("S3 upload part escaped its source file"));
        }
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| S3Error::local(format!("seek upload part cache file: {error}")))?;
        let mut sha = Sha256::new();
        let mut md5 = Md5::new();
        let mut buffer = [0u8; 1024 * 1024];
        let mut remaining = length;
        while remaining > 0 {
            let size = remaining.min(buffer.len() as u64) as usize;
            file.read_exact(&mut buffer[..size])
                .map_err(|error| S3Error::local(format!("read upload part cache file: {error}")))?;
            sha.update(&buffer[..size]);
            md5.update(&buffer[..size]);
            remaining -= size as u64;
        }
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| S3Error::local(format!("rewind upload part cache file: {error}")))?;
        let mut plan = self.plan("upload_part", args, None)?;
        plan.headers
            .insert("content-md5".to_owned(), STANDARD.encode(md5.finalize()));
        let hash = encode_lower(sha.finalize());
        let mut limited = ReadResumingReader::new(file.take(length));
        let request = self.request(
            &plan,
            ureq::SendBody::from_reader(&mut limited),
            Some((&hash, length)),
        )?;
        let response = self
            .agent
            .run(request)
            .map_err(|error| S3Error::transport(error.to_string()))?;
        self.read_response("upload_part", response)
    }

    pub fn list_versions(&self, prefix: &str) -> Result<Vec<Value>> {
        let mut args = json!({"Bucket":self.bucket,"Prefix":prefix,"MaxKeys":1000});
        let mut versions = Vec::new();
        let mut identities = BTreeSet::new();
        let mut markers = BTreeSet::new();
        for _ in 0..MAX_LIST_PAGES {
            let response = self.call_s3("list_object_versions", &args, None)?.value;
            for (section, deleted) in [("Versions", false), ("DeleteMarkers", true)] {
                for row in response[section].as_array().into_iter().flatten() {
                    let key = row["Key"]
                        .as_str()
                        .filter(|key| key.starts_with(prefix))
                        .ok_or_else(|| {
                            Error::message("S3 history listing escaped its requested prefix")
                        })?;
                    let version = row["VersionId"]
                        .as_str()
                        .filter(|version| !version.is_empty())
                        .ok_or_else(|| Error::message("S3 history contains no exact version ID"))?;
                    if !identities.insert((key.to_owned(), version.to_owned())) {
                        return Err(Error::message(
                            "S3 history listing repeated an object version",
                        ));
                    }
                    let mut row = row.clone();
                    row["delete_marker"] = Value::Bool(deleted);
                    versions.push(row);
                }
            }
            if !truncated(&response)? {
                return Ok(versions);
            }
            let key = nonempty(&response, "NextKeyMarker")?;
            let version = nonempty(&response, "NextVersionIdMarker")?;
            if !markers.insert((key.to_owned(), version.to_owned())) {
                return Err(Error::message(
                    "S3 history listing repeated pagination markers",
                ));
            }
            args["KeyMarker"] = key.into();
            args["VersionIdMarker"] = version.into();
        }
        Err(Error::message(
            "S3 history listing exceeded its pagination limit",
        ))
    }

    fn plan(&self, operation: &str, args: &Value, body: Option<&[u8]>) -> S3Result<RequestPlan> {
        let args = args
            .as_object()
            .ok_or_else(|| S3Error::local("S3 request must be an object"))?;
        if args
            .get("Bucket")
            .and_then(Value::as_str)
            .is_some_and(|bucket| bucket != self.bucket)
        {
            return Err(S3Error::local("S3 request escaped its configured bucket"));
        }
        let mut plan = RequestPlan {
            operation: operation.to_owned(),
            method: "GET",
            path: format!("{}/{}", self.endpoint_path, uri_encode(&self.bucket, true)),
            query: Vec::new(),
            headers: BTreeMap::new(),
            body: body.unwrap_or_default().to_vec(),
        };
        let object_operation = !matches!(
            operation,
            "get_bucket_versioning"
                | "list_object_versions"
                | "list_multipart_uploads"
                | "list_objects_v2"
        );
        if object_operation {
            let key = args
                .get("Key")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| S3Error::local("S3 object request has no key"))?;
            plan.path.push('/');
            plan.path.push_str(&uri_encode(key, false));
        }
        match operation {
            "get_bucket_versioning" => plan.query.push(("versioning".to_owned(), String::new())),
            "list_object_versions" => {
                plan.query.push(("versions".to_owned(), String::new()));
                query_args(
                    &mut plan,
                    args,
                    &[
                        ("Prefix", "prefix"),
                        ("MaxKeys", "max-keys"),
                        ("KeyMarker", "key-marker"),
                        ("VersionIdMarker", "version-id-marker"),
                        ("Delimiter", "delimiter"),
                    ],
                )?;
            }
            "list_multipart_uploads" => {
                plan.query.push(("uploads".to_owned(), String::new()));
                query_args(
                    &mut plan,
                    args,
                    &[
                        ("Prefix", "prefix"),
                        ("MaxUploads", "max-uploads"),
                        ("KeyMarker", "key-marker"),
                        ("UploadIdMarker", "upload-id-marker"),
                        ("Delimiter", "delimiter"),
                    ],
                )?;
            }
            "list_objects_v2" => {
                plan.query.push(("list-type".to_owned(), "2".to_owned()));
                query_args(
                    &mut plan,
                    args,
                    &[
                        ("Prefix", "prefix"),
                        ("MaxKeys", "max-keys"),
                        ("ContinuationToken", "continuation-token"),
                        ("StartAfter", "start-after"),
                        ("Delimiter", "delimiter"),
                    ],
                )?;
            }
            "head_object" => plan.method = "HEAD",
            "get_object" => {}
            "delete_object" => plan.method = "DELETE",
            "put_object" | "copy_object" => plan.method = "PUT",
            "get_object_tagging" => plan.query.push(("tagging".to_owned(), String::new())),
            "put_object_tagging" => {
                plan.method = "PUT";
                plan.query.push(("tagging".to_owned(), String::new()));
                plan.body = tagging_xml(
                    args.get("Tagging")
                        .ok_or_else(|| S3Error::local("object tagging has no TagSet"))?,
                )?;
            }
            "delete_object_tagging" => {
                plan.method = "DELETE";
                plan.query.push(("tagging".to_owned(), String::new()));
            }
            "create_multipart_upload" => {
                plan.method = "POST";
                plan.query.push(("uploads".to_owned(), String::new()));
            }
            "upload_part" | "upload_part_copy" => {
                plan.method = "PUT";
                query_args(
                    &mut plan,
                    args,
                    &[("PartNumber", "partNumber"), ("UploadId", "uploadId")],
                )?;
            }
            "complete_multipart_upload" => {
                plan.method = "POST";
                query_args(&mut plan, args, &[("UploadId", "uploadId")])?;
                plan.body = complete_xml(
                    args.get("MultipartUpload")
                        .ok_or_else(|| S3Error::local("multipart completion has no parts"))?,
                )?;
            }
            "abort_multipart_upload" => {
                plan.method = "DELETE";
                query_args(&mut plan, args, &[("UploadId", "uploadId")])?;
            }
            "list_parts" => {
                query_args(
                    &mut plan,
                    args,
                    &[
                        ("UploadId", "uploadId"),
                        ("PartNumberMarker", "part-number-marker"),
                        ("MaxParts", "max-parts"),
                    ],
                )?;
            }
            _ => {
                return Err(S3Error::local(format!(
                    "unsupported S3 operation {operation:?}"
                )));
            }
        }
        query_args(&mut plan, args, &[("VersionId", "versionId")])?;
        for (field, header) in HEADER_FIELDS {
            if let Some(value) = args.get(*field) {
                plan.headers
                    .insert((*header).to_owned(), header_value(field, value)?);
            }
        }
        if let Some(metadata) = args.get("Metadata") {
            for (key, value) in metadata
                .as_object()
                .ok_or_else(|| S3Error::local("S3 metadata must be an object"))?
            {
                if key.is_empty()
                    || !key
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
                {
                    return Err(S3Error::local(
                        "S3 metadata contains an invalid header name",
                    ));
                }
                let value = value
                    .as_str()
                    .ok_or_else(|| S3Error::local("S3 metadata values must be strings"))?;
                plan.headers.insert(
                    format!("x-amz-meta-{}", key.to_ascii_lowercase()),
                    value.to_owned(),
                );
            }
        }
        if let Some(source) = args.get("CopySource") {
            let source = if let Some(value) = source.as_str() {
                value.to_owned()
            } else {
                let source = source
                    .as_object()
                    .ok_or_else(|| S3Error::local("invalid S3 copy source"))?;
                let bucket = source
                    .get("Bucket")
                    .and_then(Value::as_str)
                    .ok_or_else(|| S3Error::local("copy source has no bucket"))?;
                if bucket != self.bucket {
                    return Err(S3Error::local("copy source escaped configured bucket"));
                }
                let key = source
                    .get("Key")
                    .and_then(Value::as_str)
                    .ok_or_else(|| S3Error::local("copy source has no key"))?;
                let mut value = format!("/{}/{}", uri_encode(bucket, true), uri_encode(key, false));
                if let Some(version) = source.get("VersionId").and_then(Value::as_str) {
                    value.push_str("?versionId=");
                    value.push_str(&uri_encode(version, true));
                }
                value
            };
            plan.headers.insert("x-amz-copy-source".to_owned(), source);
        }
        if matches!(
            operation,
            "put_object" | "upload_part" | "put_object_tagging"
        ) && !plan.headers.contains_key("content-md5")
        {
            plan.headers.insert(
                "content-md5".to_owned(),
                STANDARD.encode(Md5::digest(&plan.body)),
            );
        }
        if matches!(
            operation,
            "complete_multipart_upload" | "put_object_tagging"
        ) {
            plan.headers
                .entry("content-type".to_owned())
                .or_insert_with(|| "application/xml".to_owned());
        }
        Ok(plan)
    }

    fn run_buffered_request(
        &self,
        plan: &RequestPlan,
        body: &[u8],
    ) -> S3Result<ureq::http::Response<ureq::Body>> {
        let mut retries = 0;
        loop {
            // Rebuild and sign each attempt. Only interrupted GET/HEAD I/O
            // before a response is available can be replayed; response-body
            // validation remains outside this loop, and writes are never retried.
            let request = self.request(plan, body, None)?;
            match self.agent.run(request) {
                Ok(response) => return Ok(response),
                Err(error)
                    if matches!(plan.method, "GET" | "HEAD")
                        && retries < INTERRUPTED_READ_RETRIES
                        && matches!(&error, ureq::Error::Io(io) if io.kind() == std::io::ErrorKind::Interrupted) =>
                {
                    retries += 1;
                }
                Err(error) => return Err(S3Error::transport(error.to_string())),
            }
        }
    }

    fn request<B: ureq::AsSendBody>(
        &self,
        plan: &RequestPlan,
        body: B,
        file_hash: Option<(&str, u64)>,
    ) -> S3Result<ureq::http::Request<B>> {
        let query = canonical_query(&plan.query);
        let uri = format!(
            "{}{}{}{}",
            self.endpoint,
            plan.path,
            if query.is_empty() { "" } else { "?" },
            query
        );
        let mut headers = plan.headers.clone();
        headers.insert("host".to_owned(), self.host.clone());
        let sha256 = file_hash
            .map(|value| value.0.to_owned())
            .unwrap_or_else(|| encode_lower(Sha256::digest(&plan.body)));
        if matches!(plan.method, "PUT" | "POST") {
            headers.insert(
                "content-length".to_owned(),
                file_hash
                    .map(|value| value.1)
                    .unwrap_or(plan.body.len() as u64)
                    .to_string(),
            );
        }
        sign(
            &self.credentials,
            &self.region,
            (plan.method, &plan.path, &query),
            &mut headers,
            &sha256,
            &chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string(),
        );
        let mut request = ureq::http::Request::builder().method(plan.method).uri(uri);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        request.body(body).map_err(|_| {
            S3Error::local(format!(
                "invalid {} S3 HTTP request headers",
                plan.operation
            ))
        })
    }

    fn read_response(
        &self,
        operation: &str,
        mut response: ureq::http::Response<ureq::Body>,
    ) -> S3Result<S3Response> {
        let status = response.status().as_u16();
        let header_code = response
            .headers()
            .get("x-amz-error-code")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let header_message = response
            .headers()
            .get("x-amz-error-message")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let mut metadata = response_metadata(response.headers())?;
        let bytes = response
            .body_mut()
            .with_config()
            .limit(if operation == "get_object" {
                MEMORY_GET_LIMIT
            } else {
                XML_LIMIT
            })
            .read_to_vec()
            .map_err(|error| S3Error::transport(format!("incomplete S3 response body: {error}")))?;
        let xml = if operation != "get_object" && !bytes.is_empty() {
            Some(parse_xml(&bytes)?)
        } else if !response.status().is_success() && !bytes.is_empty() {
            parse_xml(&bytes).ok()
        } else {
            None
        };
        // CopyObject and multipart completion can carry <Error> with HTTP 200.
        if !response.status().is_success() || xml.as_ref().is_some_and(|node| node.name == "Error")
        {
            let details = xml
                .as_ref()
                .map(XmlNode::object)
                .unwrap_or_else(|| json!({}));
            let code = details["Code"]
                .as_str()
                .or(header_code.as_deref())
                .unwrap_or(match status {
                    404 => "NotFound",
                    403 => "AccessDenied",
                    412 => "PreconditionFailed",
                    409 => "Conflict",
                    501 => "NotImplemented",
                    _ => "HttpError",
                })
                .to_owned();
            let message = details["Message"]
                .as_str()
                .or(header_message.as_deref())
                .unwrap_or("S3 request was rejected")
                .to_owned();
            let header = details["Header"]
                .as_str()
                .or_else(|| details["HeaderName"].as_str())
                .map(str::to_owned);
            return Err(S3Error {
                status: Some(status),
                code,
                message,
                header,
                details,
            });
        }
        if let Some(node) = xml {
            let parsed = operation_result(operation, &node)?;
            metadata
                .as_object_mut()
                .expect("metadata object")
                .extend(parsed.as_object().expect("XML result object").clone());
        }
        if operation == "get_object" {
            let expected = metadata["ContentLength"]
                .as_u64()
                .ok_or_else(|| S3Error::local("S3 GET returned no Content-Length"))?;
            if expected != bytes.len() as u64 {
                return Err(S3Error::transport(
                    "incomplete S3 GET: Content-Length mismatch",
                ));
            }
            Ok(S3Response {
                value: metadata,
                body: bytes,
            })
        } else {
            Ok(S3Response {
                value: metadata,
                body: Vec::new(),
            })
        }
    }
}

const HEADER_FIELDS: &[(&str, &str)] = &[
    ("IfMatch", "if-match"),
    ("IfNoneMatch", "if-none-match"),
    ("Range", "range"),
    ("ContentMD5", "content-md5"),
    ("ContentType", "content-type"),
    ("CacheControl", "cache-control"),
    ("ContentDisposition", "content-disposition"),
    ("ContentEncoding", "content-encoding"),
    ("ContentLanguage", "content-language"),
    ("Expires", "expires"),
    ("WebsiteRedirectLocation", "x-amz-website-redirect-location"),
    ("StorageClass", "x-amz-storage-class"),
    ("ServerSideEncryption", "x-amz-server-side-encryption"),
    ("SSEKMSKeyId", "x-amz-server-side-encryption-aws-kms-key-id"),
    (
        "BucketKeyEnabled",
        "x-amz-server-side-encryption-bucket-key-enabled",
    ),
    ("ObjectLockMode", "x-amz-object-lock-mode"),
    (
        "ObjectLockRetainUntilDate",
        "x-amz-object-lock-retain-until-date",
    ),
    ("ObjectLockLegalHoldStatus", "x-amz-object-lock-legal-hold"),
    ("Tagging", "x-amz-tagging"),
    ("MetadataDirective", "x-amz-metadata-directive"),
    ("TaggingDirective", "x-amz-tagging-directive"),
    ("CopySourceIfMatch", "x-amz-copy-source-if-match"),
    ("CopySourceRange", "x-amz-copy-source-range"),
    ("CopySourceIfNoneMatch", "x-amz-copy-source-if-none-match"),
];

fn header_value(field: &str, value: &Value) -> S3Result<String> {
    if let Some(value) = value.as_str() {
        return Ok(value.to_owned());
    }
    if let Some(value) = value.as_bool() {
        return Ok(value.to_string());
    }
    if let Some(value) = value.as_u64() {
        return Ok(value.to_string());
    }
    Err(S3Error::local(format!(
        "S3 {field} header must be a scalar"
    )))
}

fn query_args(
    plan: &mut RequestPlan,
    args: &Map<String, Value>,
    fields: &[(&str, &str)],
) -> S3Result<()> {
    for (field, name) in fields {
        if let Some(value) = args.get(*field) {
            plan.query
                .push(((*name).to_owned(), header_value(field, value)?));
        }
    }
    Ok(())
}

pub(crate) fn uri_encode(value: &str, encode_slash: bool) -> String {
    let mut result = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'.' | b'_' | b'~')
            || (byte == b'/' && !encode_slash)
        {
            result.push(byte as char);
        } else {
            result.push('%');
            result.push(char::from(b"0123456789ABCDEF"[(byte >> 4) as usize]));
            result.push(char::from(b"0123456789ABCDEF"[(byte & 15) as usize]));
        }
    }
    result
}

fn canonical_query(query: &[(String, String)]) -> String {
    let mut query = query
        .iter()
        .map(|(key, value)| (uri_encode(key, true), uri_encode(value, true)))
        .collect::<Vec<_>>();
    query.sort();
    query
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut padded = [0u8; 64];
    let hashed;
    let key = if key.len() > 64 {
        hashed = Sha256::digest(key);
        hashed.as_slice()
    } else {
        key
    };
    padded[..key.len()].copy_from_slice(key);
    let mut inner = Sha256::new();
    inner.update(padded.map(|byte| byte ^ 0x36));
    inner.update(message);
    let mut outer = Sha256::new();
    outer.update(padded.map(|byte| byte ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().to_vec()
}

fn sign(
    credentials: &Credentials,
    region: &str,
    request: (&str, &str, &str),
    headers: &mut BTreeMap<String, String>,
    payload_hash: &str,
    date: &str,
) {
    let (method, path, query) = request;
    headers.insert("x-amz-date".to_owned(), date.to_owned());
    headers.insert("x-amz-content-sha256".to_owned(), payload_hash.to_owned());
    if let Some(token) = &credentials.token {
        headers.insert("x-amz-security-token".to_owned(), token.clone());
    }
    let canonical_headers = headers
        .iter()
        .map(|(key, value)| {
            format!(
                "{key}:{}\n",
                value.split_ascii_whitespace().collect::<Vec<_>>().join(" ")
            )
        })
        .collect::<String>();
    let signed_headers = headers.keys().cloned().collect::<Vec<_>>().join(";");
    let canonical =
        format!("{method}\n{path}\n{query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}");
    let scope = format!("{}/{region}/s3/aws4_request", &date[..8]);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{date}\n{scope}\n{}",
        encode_lower(Sha256::digest(canonical.as_bytes()))
    );
    let key = hmac_sha256(
        format!("AWS4{}", credentials.secret).as_bytes(),
        &date.as_bytes()[..8],
    );
    let key = hmac_sha256(&key, region.as_bytes());
    let key = hmac_sha256(&key, b"s3");
    let key = hmac_sha256(&key, b"aws4_request");
    let signature = encode_lower(hmac_sha256(&key, string_to_sign.as_bytes()));
    headers.insert("authorization".to_owned(),format!("AWS4-HMAC-SHA256 Credential={}/{scope},SignedHeaders={signed_headers},Signature={signature}",credentials.access));
}

fn file_hashes(file: &mut File) -> S3Result<(String, String, u64)> {
    let mut sha = Sha256::new();
    let mut md5 = Md5::new();
    let mut buffer = [0u8; 1024 * 1024];
    let mut count = 0;
    loop {
        let size = ReadResumingReader::new(&mut *file)
            .read(&mut buffer)
            .map_err(|error| S3Error::local(format!("read upload cache file: {error}")))?;
        if size == 0 {
            break;
        }
        sha.update(&buffer[..size]);
        md5.update(&buffer[..size]);
        count += size as u64;
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| S3Error::local(format!("rewind upload cache file: {error}")))?;
    Ok((
        encode_lower(sha.finalize()),
        STANDARD.encode(md5.finalize()),
        count,
    ))
}

fn response_metadata(headers: &ureq::http::HeaderMap) -> S3Result<Value> {
    let mut value = Map::new();
    let mut metadata = Map::new();
    for (name, raw) in headers {
        let name = name.as_str();
        let text = raw
            .to_str()
            .map_err(|_| S3Error::local("S3 returned a non-UTF8 metadata header"))?;
        if let Some(name) = name.strip_prefix("x-amz-meta-") {
            metadata.insert(name.to_owned(), text.into());
            continue;
        }
        let field = match name {
            "content-length" => Some("ContentLength"),
            "etag" => Some("ETag"),
            "x-amz-version-id" => Some("VersionId"),
            "x-amz-delete-marker" => Some("DeleteMarker"),
            "last-modified" => Some("LastModified"),
            "x-amz-bucket-region" => Some("BucketRegion"),
            _ => HEADER_FIELDS
                .iter()
                .find_map(|(field, header)| (*header == name).then_some(*field)),
        };
        if let Some(field) = field {
            let entry = match field {
                "ContentLength" => Value::Number(
                    text.parse::<u64>()
                        .map_err(|_| S3Error::local("S3 returned an invalid Content-Length"))?
                        .into(),
                ),
                "DeleteMarker" | "BucketKeyEnabled" => Value::Bool(match text {
                    "true" => true,
                    "false" => false,
                    _ => {
                        return Err(S3Error::local(
                            "S3 returned an invalid boolean metadata header",
                        ));
                    }
                }),
                "LastModified" => Value::String(normalize_timestamp(text)?),
                _ => Value::String(text.to_owned()),
            };
            value.insert(field.to_owned(), entry);
        }
    }
    value.insert("Metadata".to_owned(), metadata.into());
    Ok(value.into())
}

#[derive(Debug)]
struct XmlNode {
    name: String,
    text: String,
    children: Vec<XmlNode>,
}

impl XmlNode {
    fn object(&self) -> Value {
        let mut result = Map::new();
        for node in &self.children {
            result.insert(
                node.name.clone(),
                if node.children.is_empty() {
                    node.text.clone().into()
                } else {
                    node.object()
                },
            );
        }
        result.into()
    }
    fn child(&self, name: &str) -> Option<&Self> {
        self.children.iter().find(|node| node.name == name)
    }
}

fn parse_xml(raw: &[u8]) -> S3Result<XmlNode> {
    let mut reader = Reader::from_reader(raw);
    reader.config_mut().trim_text(false);
    let mut stack: Vec<XmlNode> = Vec::new();
    let mut root = None;
    loop {
        match reader
            .read_event()
            .map_err(|error| S3Error::local(format!("invalid S3 XML response: {error}")))?
        {
            Event::Start(element) => {
                if stack.len() > 64 {
                    return Err(S3Error::local("S3 XML response is too deeply nested"));
                }
                let name = element.local_name().as_ref().to_owned();
                stack.push(XmlNode {
                    name,
                    text: String::new(),
                    children: Vec::new(),
                });
            }
            Event::Empty(element) => {
                let name = element.local_name().as_ref().to_owned();
                let node = XmlNode {
                    name,
                    text: String::new(),
                    children: Vec::new(),
                };
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(node);
                } else if root.replace(node).is_some() {
                    return Err(S3Error::local("S3 XML has multiple roots"));
                }
            }
            Event::Text(text) => {
                if let Some(parent) = stack.last_mut() {
                    parent.text.push_str(
                        &quick_xml::escape::unescape(text.as_ref())
                            .map_err(|_| S3Error::local("S3 XML has an invalid entity"))?,
                    );
                }
            }
            Event::CData(text) => {
                if let Some(parent) = stack.last_mut() {
                    parent.text.push_str(text.as_ref());
                }
            }
            Event::GeneralRef(reference) => {
                if let Some(parent) = stack.last_mut() {
                    let escaped = format!("&{};", reference.as_ref());
                    parent.text.push_str(
                        &quick_xml::escape::unescape(&escaped)
                            .map_err(|_| S3Error::local("S3 XML has invalid entity"))?,
                    );
                }
            }
            Event::End(_) => {
                let node = stack
                    .pop()
                    .ok_or_else(|| S3Error::local("S3 XML has an unmatched closing tag"))?;
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(node);
                } else if root.replace(node).is_some() {
                    return Err(S3Error::local("S3 XML has multiple roots"));
                }
            }
            Event::DocType(_) => return Err(S3Error::local("S3 XML must not contain a DTD")),
            Event::Eof => break,
            _ => {}
        }
    }
    if !stack.is_empty() {
        return Err(S3Error::local("S3 XML response was truncated"));
    }
    root.ok_or_else(|| S3Error::local("S3 XML response has no root"))
}

fn operation_result(operation: &str, node: &XmlNode) -> S3Result<Value> {
    let expected = match operation {
        "get_bucket_versioning" => "VersioningConfiguration",
        "list_object_versions" => "ListVersionsResult",
        "list_multipart_uploads" => "ListMultipartUploadsResult",
        "list_objects_v2" => "ListBucketResult",
        "copy_object" => "CopyObjectResult",
        "upload_part_copy" => "CopyPartResult",
        "create_multipart_upload" => "InitiateMultipartUploadResult",
        "complete_multipart_upload" => "CompleteMultipartUploadResult",
        "get_object_tagging" => "Tagging",
        "list_parts" => "ListPartsResult",
        _ => return Ok(node.object()),
    };
    if node.name != expected {
        return Err(S3Error::local(format!(
            "S3 {operation} returned unexpected XML root {:?}",
            node.name
        )));
    }
    if matches!(operation, "copy_object" | "upload_part_copy") {
        return Ok(json!({expected:typed_object(node)?}));
    }
    if operation == "get_object_tagging" {
        return Ok(
            json!({"TagSet":node.child("TagSet").map(|set|set.children.iter().filter(|node|node.name=="Tag").map(XmlNode::object).collect::<Vec<_>>()).unwrap_or_default()}),
        );
    }
    let mut result = typed_object(node)?
        .as_object()
        .expect("typed object")
        .clone();
    for (element, section) in [
        ("Version", "Versions"),
        ("DeleteMarker", "DeleteMarkers"),
        ("Upload", "Uploads"),
        ("Contents", "Contents"),
        ("CommonPrefixes", "CommonPrefixes"),
        ("Part", "Parts"),
    ] {
        let values = node
            .children
            .iter()
            .filter(|node| node.name == element)
            .map(typed_object)
            .collect::<S3Result<Vec<_>>>()?;
        if !values.is_empty()
            || matches!(
                (operation, section),
                ("list_object_versions", "Versions" | "DeleteMarkers")
                    | ("list_multipart_uploads", "Uploads")
                    | ("list_objects_v2", "Contents")
                    | ("list_parts", "Parts")
            )
        {
            result.remove(element);
            result.insert(section.to_owned(), values.into());
        }
    }
    if operation.starts_with("list_") && !result.contains_key("IsTruncated") {
        return Err(S3Error::local("S3 listing has no IsTruncated flag"));
    }
    Ok(result.into())
}

fn typed_object(node: &XmlNode) -> S3Result<Value> {
    let mut result = Map::new();
    for child in &node.children {
        let value = if !child.children.is_empty() {
            typed_object(child)?
        } else {
            match child.name.as_str() {
                "IsTruncated" | "IsLatest" => match child.text.as_str() {
                    "true" => Value::Bool(true),
                    "false" => Value::Bool(false),
                    _ => return Err(S3Error::local("S3 XML contains an invalid boolean")),
                },
                "Size"
                | "MaxKeys"
                | "MaxUploads"
                | "KeyCount"
                | "PartNumber"
                | "NextPartNumberMarker"
                | "PartNumberMarker"
                | "MaxParts" => Value::Number(
                    child
                        .text
                        .parse::<u64>()
                        .map_err(|_| S3Error::local("S3 XML contains an invalid integer"))?
                        .into(),
                ),
                "LastModified" | "Initiated" => normalize_timestamp(&child.text)?.into(),
                _ => child.text.clone().into(),
            }
        };
        result.insert(child.name.clone(), value);
    }
    Ok(result.into())
}

/// Python's datetime.isoformat uses six fractional digits for nonzero
/// microseconds. Existing receipt bytes are immutable; normalize only freshly
/// received metadata before comparing it with the historical snapshot.
pub(crate) fn normalize_timestamp(value: &str) -> S3Result<String> {
    let date = chrono::DateTime::parse_from_rfc3339(value)
        .or_else(|_| chrono::DateTime::parse_from_rfc2822(value))
        .map_err(|_| S3Error::local("S3 history contains an invalid timezone-aware timestamp"))?
        .with_timezone(&chrono::Utc);
    let mut result = date.format("%Y-%m-%dT%H:%M:%S").to_string();
    let micros = date.timestamp_subsec_micros();
    if micros != 0 {
        result.push_str(&format!(".{micros:06}"));
    }
    result.push_str("+00:00");
    Ok(result)
}

fn xml_escape(value: &str) -> String {
    quick_xml::escape::escape(value).into_owned()
}

fn complete_xml(value: &Value) -> S3Result<Vec<u8>> {
    let mut xml =
        String::from("<CompleteMultipartUpload xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">");
    let parts = value["Parts"]
        .as_array()
        .ok_or_else(|| S3Error::local("multipart completion Parts must be a list"))?;
    let mut previous = 0;
    for part in parts {
        let number = part["PartNumber"]
            .as_u64()
            .filter(|number| *number > previous && *number <= 10000)
            .ok_or_else(|| {
                S3Error::local("multipart part numbers must be unique, ordered and in 1..10000")
            })?;
        previous = number;
        let etag = part["ETag"]
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| S3Error::local("multipart completion has no part ETag"))?;
        xml.push_str(&format!(
            "<Part><PartNumber>{number}</PartNumber><ETag>{}</ETag></Part>",
            xml_escape(etag)
        ));
    }
    xml.push_str("</CompleteMultipartUpload>");
    Ok(xml.into_bytes())
}

fn tagging_xml(value: &Value) -> S3Result<Vec<u8>> {
    let mut xml =
        String::from("<Tagging xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><TagSet>");
    for tag in value["TagSet"]
        .as_array()
        .ok_or_else(|| S3Error::local("object TagSet must be a list"))?
    {
        let key = tag["Key"]
            .as_str()
            .ok_or_else(|| S3Error::local("object tag has no key"))?;
        let value = tag["Value"]
            .as_str()
            .ok_or_else(|| S3Error::local("object tag has no value"))?;
        xml.push_str(&format!(
            "<Tag><Key>{}</Key><Value>{}</Value></Tag>",
            xml_escape(key),
            xml_escape(value)
        ));
    }
    xml.push_str("</TagSet></Tagging>");
    Ok(xml.into_bytes())
}

fn truncated(value: &Value) -> Result<bool> {
    value["IsTruncated"]
        .as_bool()
        .ok_or_else(|| Error::message("S3 listing has no valid truncation flag"))
}
fn nonempty<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value[field]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::message(format!("S3 listing has no {field} pagination marker")))
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|value| !value.is_empty())
}

fn storage_location(location: &str) -> Result<(String, String)> {
    // S3 object keys keep percent escapes and Unicode literally. HTTP URL
    // parsing would decode or rewrite the logical object prefix.
    let raw = location
        .strip_prefix("s3://")
        .ok_or_else(|| Error::message("managed storage must use s3://bucket/prefix"))?;
    let (bucket, prefix) = raw.split_once('/').unwrap_or((raw, ""));
    if bucket.is_empty() || bucket.contains(['@', ':', '?', '#']) || raw.contains(['?', '#']) {
        return Err(Error::message(
            "managed storage must use a plain s3://bucket/prefix URL",
        ));
    }
    Ok((bucket.to_owned(), prefix.trim_end_matches('/').to_owned()))
}

fn b2_region(endpoint: &str) -> Option<String> {
    let parsed = Url::parse(endpoint).ok()?;
    let host = parsed.host_str()?;
    if !host.ends_with(".backblazeb2.com") {
        return None;
    }
    let region = host.strip_prefix("s3.")?.strip_suffix(".backblazeb2.com")?;
    (!region.is_empty()).then(|| region.to_owned())
}

fn ini_section(raw: &str, section: &str) -> BTreeMap<String, String> {
    let mut found = BTreeMap::new();
    let mut active = false;
    for line in raw.lines().map(str::trim) {
        if line.starts_with('[') && line.ends_with(']') {
            let name = line[1..line.len() - 1].trim();
            let name = if name.len() >= 2
                && ((name.starts_with('\'') && name.ends_with('\''))
                    || (name.starts_with('"') && name.ends_with('"')))
            {
                &name[1..name.len() - 1]
            } else {
                name
            };
            active = name == section;
        } else if active
            && !line.starts_with('#')
            && !line.starts_with(';')
            && let Some((key, value)) = line.split_once('=')
        {
            found.insert(
                key.trim().to_ascii_lowercase(),
                value.trim().trim_matches('"').to_owned(),
            );
        }
    }
    found
}

#[cfg(test)]
fn is_legacy_cas_configuration(raw: &str) -> bool {
    is_legacy_cas_configuration_with_public_route(raw, false)
}

pub(crate) fn is_legacy_cas_configuration_with_public_route(raw: &str, public_route: bool) -> bool {
    let strict_section = |section: &str| -> Option<BTreeMap<String, String>> {
        let mut fields = BTreeMap::new();
        let mut active = false;
        let mut seen = false;
        for line in raw.lines().map(str::trim) {
            if line.is_empty() || line.starts_with(['#', ';']) {
                continue;
            }
            if line.starts_with('[') {
                let name = line.strip_prefix('[')?.strip_suffix(']')?.trim();
                let name = if name.len() >= 2
                    && ((name.starts_with('\'') && name.ends_with('\''))
                        || (name.starts_with('"') && name.ends_with('"')))
                {
                    &name[1..name.len() - 1]
                } else {
                    name
                };
                active = name == section;
                if active {
                    if seen {
                        return None;
                    }
                    seen = true;
                }
            } else if active {
                let (key, value) = line.split_once('=')?;
                let key = key.trim().to_ascii_lowercase();
                let value = value.trim();
                if key.is_empty()
                    || (value.starts_with('"') != value.ends_with('"'))
                    || fields
                        .insert(key, value.trim_matches('"').to_owned())
                        .is_some()
                {
                    return None;
                }
            }
        }
        seen.then_some(fields)
    };
    let Some(core) = strict_section("core") else {
        return false;
    };
    let Some(remote) = core
        .get("remote")
        .filter(|remote| !remote.trim().is_empty())
    else {
        return false;
    };
    let Some(settings) = strict_section(&format!("remote \"{remote}\"")) else {
        return false;
    };
    (public_route
        || settings
            .get("url")
            .is_some_and(|url| storage_location(url).is_ok()))
        && settings
            .get("version_aware")
            .is_none_or(|value| value.eq_ignore_ascii_case("false"))
}

fn read_optional(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(raw) => Ok(raw),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(source) => Err(Error::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Legacy source reader used only by repository import and old historical
/// checkouts. Native repositories never consult these files.
pub(crate) fn legacy_remote_settings(
    root: &Path,
    remote_name: &str,
) -> Result<BTreeMap<String, String>> {
    let mut settings = BTreeMap::new();
    for file in [".dvc/config", ".dvc/config.local"] {
        crate::path::reject_symlink_traversal(root, file, "legacy storage configuration")?;
        settings.extend(ini_section(
            &read_optional(&root.join(file))?,
            &format!("remote \"{remote_name}\""),
        ));
    }
    Ok(settings)
}

pub(crate) fn legacy_s3_configuration(
    root: &Path,
) -> Result<Option<(String, Option<String>, CredentialsConfig)>> {
    let mut core = BTreeMap::new();
    for file in [".dvc/config", ".dvc/config.local"] {
        crate::path::reject_symlink_traversal(root, file, "legacy storage configuration")?;
        core.extend(ini_section(&read_optional(&root.join(file))?, "core"));
    }
    let remote_name = core
        .get("remote")
        .map(String::as_str)
        .unwrap_or("workspace-mgr");
    let settings = legacy_remote_settings(root, remote_name)?;
    let Some(location) = settings.get("url").cloned() else {
        return Ok(None);
    };
    let endpoint = settings.get("endpointurl").cloned();
    let credentials = CredentialsConfig::from_legacy_settings(&settings)?;
    Ok(Some((location, endpoint, credentials)))
}

fn profile_settings(profile: &str) -> Result<(BTreeMap<String, String>, BTreeMap<String, String>)> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| Error::message("cannot resolve AWS profile directory: HOME is unset"))?;
    let config = env_nonempty("AWS_CONFIG_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".aws/config"));
    let credentials = env_nonempty("AWS_SHARED_CREDENTIALS_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".aws/credentials"));
    Ok((
        ini_section(
            &read_optional(&config)?,
            &if profile == "default" {
                profile.to_owned()
            } else {
                format!("profile {profile}")
            },
        ),
        ini_section(&read_optional(&credentials)?, profile),
    ))
}

fn resolve_credentials(
    remote: &BTreeMap<String, String>,
    saved: &BTreeMap<String, String>,
    config: &BTreeMap<String, String>,
) -> Result<Credentials> {
    if remote.contains_key("access_key_id") || remote.contains_key("secret_access_key") {
        return credentials_from(
            remote.get("access_key_id").cloned(),
            remote.get("secret_access_key").cloned(),
            remote.get("session_token").cloned(),
            "local S3 credentials",
        );
    }
    if env_nonempty("AWS_ACCESS_KEY_ID").is_some()
        || env_nonempty("AWS_SECRET_ACCESS_KEY").is_some()
    {
        return credentials_from(
            env_nonempty("AWS_ACCESS_KEY_ID"),
            env_nonempty("AWS_SECRET_ACCESS_KEY"),
            env_nonempty("AWS_SESSION_TOKEN").or_else(|| env_nonempty("AWS_SECURITY_TOKEN")),
            "AWS environment",
        );
    }
    if let Some(command) = remote.get("credential_process") {
        return process_credentials(command);
    }
    let mut profile = config.clone();
    profile.extend(saved.clone());
    if [
        "role_arn",
        "web_identity_token_file",
        "sso_session",
        "sso_start_url",
        "credential_source",
    ]
    .iter()
    .any(|key| profile.contains_key(*key))
    {
        return Err(Error::message(
            "this AWS profile requires federated or role credentials; provide the resolved temporary AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY/AWS_SESSION_TOKEN, or configure credential_process in a separate profile",
        ));
    }
    if profile.contains_key("aws_access_key_id") || profile.contains_key("aws_secret_access_key") {
        return credentials_from(
            profile.get("aws_access_key_id").cloned(),
            profile.get("aws_secret_access_key").cloned(),
            profile.get("aws_session_token").cloned(),
            "AWS shared profile",
        );
    }
    if let Some(command) = profile.get("credential_process") {
        return process_credentials(command);
    }
    Err(Error::message(
        "S3 credentials are unavailable; configure AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY (and AWS_SESSION_TOKEN for temporary credentials), an AWS_PROFILE with shared credentials, or .workspace-mgr/local/credentials.toml",
    ))
}

/// AWS credential_process is an argv command, not shell code. Expansion,
/// substitution and pipe evaluation must never be introduced by the client.
fn command_words(command: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut started = false;
    for character in command.chars() {
        if escaped {
            word.push(character);
            escaped = false;
            started = true;
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            escaped = true;
            started = true;
            continue;
        }
        if let Some(delimiter) = quote {
            if character == delimiter {
                quote = None;
            } else {
                word.push(character);
            }
            started = true;
            continue;
        }
        if character == '\'' || character == '"' {
            quote = Some(character);
            started = true;
        } else if character.is_ascii_whitespace() {
            if started {
                words.push(std::mem::take(&mut word));
                started = false;
            }
        } else {
            word.push(character);
            started = true;
        }
    }
    if quote.is_some() || escaped {
        return Err(Error::message(
            "AWS credential_process contains unterminated quoting",
        ));
    }
    if started {
        words.push(word);
    }
    if words.first().is_none_or(String::is_empty) {
        return Err(Error::message("AWS credential_process has no program"));
    }
    Ok(words)
}

fn process_credentials(command: &str) -> Result<Credentials> {
    let words = command_words(command)?;
    let output = std::process::Command::new(&words[0])
        .args(&words[1..])
        .output()
        .map_err(|_| Error::message("AWS credential_process could not start"))?;
    if !output.status.success() {
        return Err(Error::message(
            "AWS credential_process failed; its output is withheld because it may contain credentials",
        ));
    }
    let value: Value = serde_json::from_slice(&output.stdout).map_err(|_| {
        Error::message("AWS credential_process returned invalid JSON; its output is withheld")
    })?;
    if value["Version"] != 1 {
        return Err(Error::message(
            "AWS credential_process returned an unsupported credentials schema",
        ));
    }
    if let Some(expiration) = value["Expiration"].as_str() {
        let expiration = chrono::DateTime::parse_from_rfc3339(expiration)
            .map_err(|_| Error::message("AWS credential_process returned an invalid expiration"))?;
        if expiration <= chrono::Utc::now() {
            return Err(Error::message(
                "AWS credential_process returned expired credentials",
            ));
        }
    }
    credentials_from(
        value["AccessKeyId"].as_str().map(str::to_owned),
        value["SecretAccessKey"].as_str().map(str::to_owned),
        value["SessionToken"].as_str().map(str::to_owned),
        "AWS credential_process",
    )
}

fn credentials_from(
    access: Option<String>,
    secret: Option<String>,
    token: Option<String>,
    origin: &str,
) -> Result<Credentials> {
    let (access, secret) = match (
        access.filter(|value| !value.is_empty()),
        secret.filter(|value| !value.is_empty()),
    ) {
        (Some(access), Some(secret)) => (access, secret),
        _ => {
            return Err(Error::message(format!(
                "{origin} credentials are incomplete; access and secret keys must be configured together"
            )));
        }
    };
    Ok(Credentials {
        access,
        secret,
        token,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn historical_cas_layout_requires_unambiguous_valid_remote_configuration() {
        let raw = "[core]\nremote = archive\n['remote \"archive\"']\nurl = s3://fixture/cas\nversion_aware = false\n";
        assert!(is_legacy_cas_configuration(raw));
        assert!(is_legacy_cas_configuration(
            &raw.replace("version_aware = false\n", "")
        ));
        let layout_only = raw.replace("url = s3://fixture/cas\n", "");
        assert!(!is_legacy_cas_configuration(&layout_only));
        assert!(is_legacy_cas_configuration_with_public_route(
            &layout_only,
            true
        ));
        assert!(!is_legacy_cas_configuration_with_public_route(
            "[core]\nremote = archive\n",
            true
        ));
        for malformed in [
            raw.replace("false", "true"),
            raw.replace("false", "perhaps"),
            raw.replace("remote = archive", "remote = archive\nremote = other"),
            format!("{raw}version_aware = true\n"),
            format!("{raw}url = s3://other/cas\n"),
            format!("{raw}['remote \"archive\"']\nversion_aware = false\n"),
            raw.replace("s3://fixture/cas", "../filesystem"),
            raw.replace("[core]\nremote = archive\n", ""),
        ] {
            assert!(!is_legacy_cas_configuration(&malformed));
        }
    }
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    };
    use std::thread;
    use ureq::unversioned::resolver::DefaultResolver;
    use ureq::unversioned::transport::{
        Buffers, ConnectionDetails, Connector, DefaultConnector, NextTimeout, Transport,
    };

    #[derive(Debug, Clone, Copy)]
    enum InjectedFailure {
        Io(std::io::ErrorKind),
        Timeout,
    }

    #[derive(Debug)]
    struct FailingConnector {
        attempts: Arc<AtomicUsize>,
        failures: usize,
        failure: InjectedFailure,
        delegate: DefaultConnector,
    }

    impl Connector for FailingConnector {
        type Out = Box<dyn Transport>;

        fn connect(
            &self,
            details: &ConnectionDetails,
            chained: Option<()>,
        ) -> std::result::Result<Option<Self::Out>, ureq::Error> {
            if self.attempts.fetch_add(1, Ordering::SeqCst) < self.failures {
                return Err(match self.failure {
                    InjectedFailure::Io(kind) => {
                        ureq::Error::Io(std::io::Error::new(kind, "injected connection failure"))
                    }
                    InjectedFailure::Timeout => ureq::Error::Timeout(ureq::Timeout::Connect),
                });
            }
            self.delegate.connect(details, chained)
        }
    }

    fn inject_connector_failure(
        mut client: S3Client,
        failures: usize,
        failure: InjectedFailure,
    ) -> (S3Client, Arc<AtomicUsize>) {
        let attempts = Arc::new(AtomicUsize::new(0));
        client.agent = ureq::Agent::with_parts(
            client.agent.config().clone(),
            FailingConnector {
                attempts: attempts.clone(),
                failures,
                failure,
                delegate: DefaultConnector::default(),
            },
            DefaultResolver::default(),
        );
        (client, attempts)
    }

    #[derive(Debug)]
    struct InterruptResponseConnector {
        target: String,
        injected: Arc<AtomicUsize>,
        request_received: Arc<Mutex<mpsc::Receiver<()>>>,
        delegate: DefaultConnector,
    }

    impl Connector for InterruptResponseConnector {
        type Out = Box<dyn Transport>;

        fn connect(
            &self,
            details: &ConnectionDetails,
            chained: Option<()>,
        ) -> std::result::Result<Option<Self::Out>, ureq::Error> {
            let interrupt = details.uri.path_and_query().map(|value| value.as_str())
                == Some(self.target.as_str());
            Ok(self.delegate.connect(details, chained)?.map(|delegate| {
                Box::new(InterruptResponseTransport {
                    delegate,
                    interrupt,
                    sent: false,
                    injected: self.injected.clone(),
                    request_received: self.request_received.clone(),
                }) as Box<dyn Transport>
            }))
        }
    }

    #[derive(Debug)]
    struct InterruptResponseTransport {
        delegate: Box<dyn Transport>,
        interrupt: bool,
        sent: bool,
        injected: Arc<AtomicUsize>,
        request_received: Arc<Mutex<mpsc::Receiver<()>>>,
    }

    impl Transport for InterruptResponseTransport {
        fn buffers(&mut self) -> &mut dyn Buffers {
            self.delegate.buffers()
        }

        fn transmit_output(
            &mut self,
            amount: usize,
            timeout: NextTimeout,
        ) -> std::result::Result<(), ureq::Error> {
            self.delegate.transmit_output(amount, timeout)?;
            self.sent = true;
            Ok(())
        }

        fn await_input(&mut self, timeout: NextTimeout) -> std::result::Result<bool, ureq::Error> {
            if self.interrupt
                && self.sent
                && self
                    .injected
                    .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            {
                // The server must have parsed this real request before its
                // response is interrupted. This avoids a connect-only fault or
                // a race in which a reset discards an unobserved request.
                self.request_received
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .expect("fixture never received the request before the injected interruption");
                return Err(ureq::Error::Io(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "injected response-header interruption after request transmission",
                )));
            }
            self.delegate.await_input(timeout)
        }

        fn is_open(&mut self) -> bool {
            self.delegate.is_open()
        }

        fn is_tls(&self) -> bool {
            self.delegate.is_tls()
        }
    }

    pub(crate) fn interrupt_response_once(
        client: S3Client,
        target: String,
        request_received: mpsc::Receiver<()>,
    ) -> (S3Client, Arc<AtomicUsize>) {
        inject_response_interruption(client, target, request_received, false)
    }

    fn inject_response_interruption(
        mut client: S3Client,
        target: String,
        request_received: mpsc::Receiver<()>,
        resume_read: bool,
    ) -> (S3Client, Arc<AtomicUsize>) {
        let injected = Arc::new(AtomicUsize::new(0));
        let connector = InterruptResponseConnector {
            target,
            injected: injected.clone(),
            request_received: Arc::new(Mutex::new(request_received)),
            delegate: DefaultConnector::default(),
        };
        let config = client.agent.config().clone();
        client.agent = if resume_read {
            ureq::Agent::with_parts(
                config,
                ReadResumingConnector::new(connector),
                DefaultResolver::default(),
            )
        } else {
            ureq::Agent::with_parts(config, connector, DefaultResolver::default())
        };
        (client, injected)
    }

    #[derive(Debug)]
    pub(crate) struct WireRequest {
        pub method: String,
        pub target: String,
        pub headers: BTreeMap<String, String>,
        pub body: Vec<u8>,
    }
    #[derive(Clone)]
    pub(crate) struct Reply {
        pub status: u16,
        pub headers: Vec<(&'static str, String)>,
        pub body: Vec<u8>,
    }
    impl Reply {
        pub fn xml(body: &str) -> Self {
            Self {
                status: 200,
                headers: Vec::new(),
                body: body.as_bytes().to_vec(),
            }
        }
    }

    #[derive(Debug)]
    pub(crate) struct ReadAttempt {
        pub request: WireRequest,
        pub response_sent: bool,
    }

    pub(crate) struct RoutedFixture {
        stop: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<Vec<ReadAttempt>>>,
    }

    impl RoutedFixture {
        pub fn finish(mut self) -> Vec<ReadAttempt> {
            self.stop.store(true, Ordering::SeqCst);
            self.worker.take().unwrap().join().unwrap()
        }

        pub fn finish_requests(self) -> Vec<WireRequest> {
            self.finish()
                .into_iter()
                .map(|attempt| attempt.request)
                .collect()
        }
    }

    impl Drop for RoutedFixture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(worker) = self.worker.take() {
                let result = worker.join();
                // Stop on a client assertion failure without double-panicking,
                // but never hide a server failure when the test is succeeding.
                if !thread::panicking() {
                    result.unwrap();
                }
            }
        }
    }

    /// Read-only fixture routed by request identity, not connection order.
    /// Abandoned reads remain recorded and never consume another version's reply.
    pub(crate) fn replayable_read_fixture(
        handler: impl Fn(&WireRequest) -> Arc<Reply> + Send + Sync + 'static,
    ) -> (S3Client, RoutedFixture) {
        response_fixture(move |request| {
            assert!(matches!(request.method.as_str(), "GET" | "HEAD"));
            handler(request)
        })
    }

    /// Route every request until the caller finishes the logical operation.
    /// Read retries never consume a fixed budget or another request's reply.
    pub(crate) fn routed_fixture(
        handler: impl Fn(&WireRequest) -> Reply + Send + Sync + 'static,
    ) -> (S3Client, RoutedFixture) {
        response_fixture(move |request| Arc::new(handler(request)))
    }

    fn response_fixture(
        handler: impl Fn(&WireRequest) -> Arc<Reply> + Send + Sync + 'static,
    ) -> (S3Client, RoutedFixture) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = stop.clone();
        let handler = Arc::new(handler);
        let worker = thread::spawn(move || {
            let mut handlers = Vec::new();
            // Clients can parse large JSON bodies between requests while other
            // tests occupy the runner. Keep a bounded idle deadline without
            // counting that CPU work as a failed request.
            let idle_timeout = Duration::from_secs(60);
            let mut deadline = std::time::Instant::now() + idle_timeout;
            while !server_stop.load(Ordering::SeqCst) {
                let (mut connection, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        // Response transmission and concurrent handler work do
                        // not count toward the bounded idle-request deadline.
                        if handlers
                            .iter()
                            .any(|worker: &thread::JoinHandle<_>| !worker.is_finished())
                        {
                            deadline = std::time::Instant::now() + idle_timeout;
                        }
                        assert!(
                            std::time::Instant::now() < deadline,
                            "mock S3 routed fixture timed out"
                        );
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("mock accept: {error}"),
                };
                deadline = std::time::Instant::now() + idle_timeout;
                let handler = handler.clone();
                handlers.push(thread::spawn(move || {
                    connection.set_nonblocking(false).unwrap();
                    connection
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    connection
                        .set_write_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let request = match read_request_result(&mut connection) {
                        Ok(request) => request,
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::UnexpectedEof
                                    | std::io::ErrorKind::ConnectionReset
                            ) =>
                        {
                            return None;
                        }
                        Err(error) => panic!("mock read: {error}"),
                    };
                    let reply = handler(&request);
                    let response_sent = write_fixture_response(&mut connection, &request, &reply)
                        .unwrap_or_else(|error| panic!("mock response: {error}"));
                    Some(ReadAttempt {
                        request,
                        response_sent,
                    })
                }));
            }
            // Join every accepted connection before returning; preserve accept
            // order without serializing handlers or holding a shared log lock.
            let mut attempts = Vec::new();
            let mut failure = None;
            for handler in handlers {
                match handler.join() {
                    Ok(Some(attempt)) => attempts.push(attempt),
                    Ok(None) => {}
                    Err(error) if failure.is_none() => failure = Some(error),
                    Err(_) => {}
                }
            }
            if let Some(error) = failure {
                std::panic::resume_unwind(error);
            }
            attempts
        });
        (
            client(&endpoint),
            RoutedFixture {
                stop,
                worker: Some(worker),
            },
        )
    }

    fn write_fixture_response(
        output: &mut impl Write,
        request: &WireRequest,
        reply: &Reply,
    ) -> std::io::Result<bool> {
        let mut headers = format!("HTTP/1.1 {} Fixture\r\nConnection: close\r\n", reply.status);
        if !reply
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        {
            headers.push_str(&format!("Content-Length: {}\r\n", reply.body.len()));
        }
        for (name, value) in &reply.headers {
            headers.push_str(&format!("{name}: {value}\r\n"));
        }
        headers.push_str("\r\n");
        match output
            .write_all(headers.as_bytes())
            .and_then(|()| output.write_all(&reply.body))
        {
            Ok(()) => Ok(true),
            Err(error)
                if matches!(request.method.as_str(), "GET" | "HEAD")
                    && matches!(
                        error.kind(),
                        std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                    ) =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    #[test]
    fn fixture_response_tolerates_only_abandoned_read_connections() {
        struct FailingWriter {
            kind: std::io::ErrorKind,
            successful_writes: usize,
        }
        impl Write for FailingWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.successful_writes == 0 {
                    return Err(std::io::Error::from(self.kind));
                }
                self.successful_writes -= 1;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        for kind in [
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::TimedOut,
        ] {
            for method in ["GET", "HEAD", "DELETE", "PUT"] {
                for successful_writes in [0, 1] {
                    let request = WireRequest {
                        method: method.into(),
                        target: "/fixture".into(),
                        headers: BTreeMap::new(),
                        body: Vec::new(),
                    };
                    let mut output = FailingWriter {
                        kind,
                        successful_writes,
                    };
                    let result = write_fixture_response(&mut output, &request, &Reply::xml("body"));
                    if matches!(method, "GET" | "HEAD")
                        && matches!(
                            kind,
                            std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                        )
                    {
                        assert!(!result.unwrap());
                    } else {
                        assert_eq!(result.unwrap_err().kind(), kind);
                    }
                }
            }
        }
    }

    #[test]
    fn routed_fixture_records_an_abandoned_get_retry_and_one_subsequent_delete() {
        let (received, receiver) = mpsc::channel();
        let (client, worker) = routed_fixture(move |request| {
            if request.method == "GET" {
                received.send(()).unwrap();
                Reply {
                    status: 200,
                    headers: vec![
                        ("x-amz-version-id", "v1".into()),
                        ("ETag", "\"abc\"".into()),
                    ],
                    body: b"abc".to_vec(),
                }
            } else {
                assert_eq!(request.method, "DELETE");
                Reply {
                    status: 204,
                    headers: Vec::new(),
                    body: Vec::new(),
                }
            }
        });
        let (client, injected) = interrupt_response_once(
            client,
            "/fixture-bucket/root/read?versionId=v1".into(),
            receiver,
        );
        let result = client
            .call_s3(
                "get_object",
                &json!({"Key":"root/read","VersionId":"v1"}),
                None,
            )
            .unwrap();
        assert_eq!(result.body, b"abc");
        assert_eq!(result.value["VersionId"], "v1");
        client
            .call_s3(
                "delete_object",
                &json!({"Key":"root/read","VersionId":"v1"}),
                None,
            )
            .unwrap();
        assert_eq!(injected.load(Ordering::SeqCst), 1);
        let attempts = worker.finish();
        assert!(attempts.len() >= 3);
        assert!(
            attempts
                .iter()
                .filter(|attempt| attempt.request.method == "GET")
                .count()
                >= 2
        );
        let deletes = attempts
            .iter()
            .filter(|attempt| attempt.request.method == "DELETE")
            .collect::<Vec<_>>();
        assert_eq!(deletes.len(), 1);
        assert!(deletes[0].response_sent);
        assert!(
            attempts
                .iter()
                .all(|attempt| attempt.request.target == "/fixture-bucket/root/read?versionId=v1")
        );
    }

    #[test]
    fn routed_fixture_runs_distinct_read_handlers_concurrently() {
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let server_gate = gate.clone();
        let (entered, received) = mpsc::channel();
        let (client, worker) = routed_fixture(move |request| {
            assert_eq!(request.method, "GET");
            assert!(matches!(
                request.target.as_str(),
                "/fixture-bucket/root/a?versionId=v1" | "/fixture-bucket/root/b?versionId=v1"
            ));
            entered.send(request.target.clone()).unwrap();
            let (lock, condition) = &*server_gate;
            let (open, timeout) = condition
                .wait_timeout_while(lock.lock().unwrap(), Duration::from_secs(10), |open| !*open)
                .unwrap();
            assert!(
                !timeout.timed_out() && *open,
                "concurrent fixture handlers never entered"
            );
            drop(open);
            Reply {
                status: 200,
                headers: vec![("x-amz-version-id", "v1".into())],
                body: b"abc".to_vec(),
            }
        });
        let clients = ["root/a", "root/b"].map(|key| {
            let client = client.clone();
            thread::spawn(move || {
                client
                    .call_s3("get_object", &json!({"Key":key,"VersionId":"v1"}), None)
                    .unwrap()
            })
        });
        let mut entered_targets = std::collections::BTreeSet::new();
        while entered_targets.len() < 2 {
            entered_targets.insert(
                received
                    .recv_timeout(Duration::from_secs(10))
                    .expect("both distinct handlers must enter before either response is released"),
            );
        }
        let (lock, condition) = &*gate;
        *lock.lock().unwrap() = true;
        condition.notify_all();
        for client in clients {
            assert_eq!(client.join().unwrap().body, b"abc");
        }
        let attempts = worker.finish();
        assert!(attempts.len() >= 2);
        for target in entered_targets {
            assert!(
                attempts
                    .iter()
                    .any(|attempt| { attempt.request.target == target && attempt.response_sent })
            );
        }
    }

    fn test_credentials() -> Credentials {
        Credentials {
            access: "AKIAIOSFODNN7EXAMPLE".to_owned(),
            secret: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_owned(),
            token: None,
        }
    }
    pub(crate) fn client(endpoint: &str) -> S3Client {
        S3Client::new(
            "fixture-bucket",
            "root",
            endpoint,
            "us-east-1",
            test_credentials(),
        )
        .unwrap()
    }
    pub(crate) fn configure_repo(client: &S3Client, repo: &GitRepo) {
        if !repo.root.join(".git").exists() {
            repo.run(["init", "-q", "-b", "main"]).unwrap();
        }
        let mut config = if Config::path(repo).exists() {
            Config::load(repo).unwrap()
        } else {
            Config::default()
        };
        config.s3 = Some(crate::config::S3Config {
            url: format!("s3://{}/{}", client.bucket, client.prefix),
            endpoint_url: Some(client.endpoint.clone()),
        });
        fs::write(Config::path(repo), config.render().unwrap()).unwrap();
        fs::create_dir_all(repo.root.join(".workspace-mgr/local")).unwrap();
        fs::write(repo.root.join(CREDENTIALS_NAME), "access_key_id = \"fixture-test-only\"\nsecret_access_key = \"fixture-test-only-secret\"\nregion = \"us-east-1\"\n").unwrap();
    }
    /// Repeat one response only for the same request identity. Reads may be
    /// replayed; a second mutation always fails the fixture.
    pub(crate) fn single_reply_fixture(reply: Reply) -> (S3Client, RoutedFixture) {
        let reply = Arc::new(reply);
        let identity = Mutex::new(None);
        response_fixture(move |request| {
            let mut seen = identity.lock().unwrap();
            let requested = (request.method.clone(), request.target.clone());
            if let Some(previous) = seen.as_ref() {
                assert_eq!(
                    previous, &requested,
                    "single-response fixture received another request identity"
                );
                assert!(
                    matches!(request.method.as_str(), "GET" | "HEAD"),
                    "single-response fixture received a duplicate mutation"
                );
            } else {
                *seen = Some(requested);
            }
            drop(seen);
            reply.clone()
        })
    }

    pub(crate) fn empty_fixture() -> (S3Client, RoutedFixture) {
        routed_fixture(|request| {
            panic!("empty S3 fixture received an unexpected request: {request:?}")
        })
    }

    fn read_request_result(stream: &mut TcpStream) -> std::io::Result<WireRequest> {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let mut parts = line.split_ascii_whitespace();
        let missing = || {
            std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "incomplete mock HTTP request",
            )
        };
        let method = parts.next().ok_or_else(missing)?.to_owned();
        let target = parts.next().ok_or_else(missing)?.to_owned();
        let mut headers = BTreeMap::new();
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                return Err(missing());
            }
            if line == "\r\n" {
                break;
            }
            let (name, value) = line.trim_end().split_once(':').ok_or_else(missing)?;
            headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
        }
        let length = headers
            .get("content-length")
            .map(|value| {
                value
                    .parse::<usize>()
                    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
            })
            .transpose()?
            .unwrap_or(0);
        let mut body = vec![0; length];
        reader.read_exact(&mut body)?;
        Ok(WireRequest {
            method,
            target,
            headers,
            body,
        })
    }

    #[test]
    fn official_aws_sigv4_range_get_vector() {
        // AWS published S3 example, fixed credentials/time, no real bucket.
        // https://docs.aws.amazon.com/AmazonS3/latest/developerguide/sig-v4-header-based-auth.html
        let mut headers = BTreeMap::from([
            (
                "host".to_owned(),
                "examplebucket.s3.amazonaws.com".to_owned(),
            ),
            ("range".to_owned(), "bytes=0-9".to_owned()),
        ]);
        sign(
            &test_credentials(),
            "us-east-1",
            ("GET", "/test.txt", ""),
            &mut headers,
            &encode_lower(Sha256::digest(b"")),
            "20130524T000000Z",
        );
        assert_eq!(
            headers["authorization"],
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request,SignedHeaders=host;range;x-amz-content-sha256;x-amz-date,Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn hmac_rfc4231_vectors() {
        assert_eq!(
            encode_lower(hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            encode_lower(hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn uri_and_query_encoding_preserve_literal_key() {
        assert_eq!(uri_encode("a//b/é ?%+", false), "a//b/%C3%A9%20%3F%25%2B");
        assert_eq!(
            canonical_query(&[
                ("versions".into(), "".into()),
                ("key-marker".into(), "a/+v==".into())
            ]),
            "key-marker=a%2F%2Bv%3D%3D&versions="
        );
        let plan = client("http://127.0.0.1:1")
            .plan(
                "get_object",
                &json!({"Key":"root/a//b/é ?%+","VersionId":"a/+="}),
                None,
            )
            .unwrap();
        assert_eq!(plan.path, "/fixture-bucket/root/a//b/%C3%A9%20%3F%25%2B");
        assert_eq!(canonical_query(&plan.query), "versionId=a%2F%2B%3D");
    }

    #[test]
    fn conditional_put_has_only_supported_fixed_length_checksums() {
        let (client, worker) = single_reply_fixture(Reply {
            status: 200,
            headers: vec![
                ("x-amz-version-id", "written-version".into()),
                ("ETag", "\"abc\"".into()),
            ],
            body: Vec::new(),
        });
        let response=client.call_s3("put_object",&json!({"Key":"root/task/receipt","IfNoneMatch":"*","ContentType":"application/json","Metadata":{"proof":"bound"}}),Some(b"receipt")).unwrap();
        assert_eq!(response.value["VersionId"], "written-version");
        let requests = worker.finish_requests();
        let request = &requests[0];
        assert_eq!(request.method, "PUT");
        assert_eq!(request.headers["if-none-match"], "*");
        assert_eq!(request.headers["content-length"], "7");
        assert_eq!(
            request.headers["content-md5"],
            STANDARD.encode(Md5::digest(b"receipt"))
        );
        assert_eq!(request.body, b"receipt");
        assert!(request.headers["authorization"].contains("if-none-match"));
        assert!(
            !request
                .headers
                .keys()
                .any(|name| name.starts_with("x-amz-checksum")
                    || name == "x-amz-sdk-checksum-algorithm"
                    || name == "trailer"
                    || name == "transfer-encoding")
        );
    }

    #[test]
    fn exact_get_metadata_binary_body_and_duplicate_slashes() {
        let bytes = vec![0, 255, 4, 13, 10];
        let (client, worker) = single_reply_fixture(Reply {
            status: 200,
            headers: vec![
                ("x-amz-version-id", "v/+=1".into()),
                ("ETag", "\"abc\"".into()),
                ("Last-Modified", "Fri, 24 May 2013 00:00:00 GMT".into()),
                ("x-amz-meta-proof", "value".into()),
            ],
            body: bytes.clone(),
        });
        let response = client
            .call_s3(
                "get_object",
                &json!({"Key":"root/a//b","VersionId":"v/+=1","IfMatch":"\"abc\""}),
                None,
            )
            .unwrap();
        assert_eq!(response.body, bytes);
        assert_eq!(response.value["ContentLength"], 5);
        assert_eq!(response.value["VersionId"], "v/+=1");
        assert_eq!(response.value["Metadata"]["proof"], "value");
        assert_eq!(response.value["LastModified"], "2013-05-24T00:00:00+00:00");
        let request = worker.finish_requests().pop().unwrap();
        assert_eq!(
            request.target,
            "/fixture-bucket/root/a//b?versionId=v%2F%2B%3D1"
        );
        assert_eq!(request.headers["if-match"], "\"abc\"");
    }

    #[test]
    fn streaming_download_and_upload_use_fixed_length_bytes() {
        streaming_roundtrip(false);
    }

    #[test]
    fn streaming_upload_resumes_interrupted_response_without_replaying_request() {
        streaming_roundtrip(true);
    }

    fn streaming_roundtrip(interrupted: bool) {
        let bytes = vec![42u8; 2 * 1024 * 1024 + 17];
        let download_bytes = bytes.clone();
        let (request_sent, request_received) = mpsc::channel();
        let (client, worker) = routed_fixture(move |request| {
            match (request.method.as_str(), request.target.as_str()) {
                ("GET", "/fixture-bucket/root/data?versionId=read") => Reply {
                    status: 200,
                    headers: vec![("x-amz-version-id", "read".into())],
                    body: download_bytes.clone(),
                },
                ("PUT", "/fixture-bucket/root/other") => {
                    if interrupted {
                        request_sent.send(()).unwrap();
                    }
                    Reply {
                        status: 200,
                        headers: vec![("x-amz-version-id", "written".into())],
                        body: Vec::new(),
                    }
                }
                _ => panic!("unexpected streaming request: {request:?}"),
            }
        });
        let (client, injected) = if interrupted {
            inject_response_interruption(
                client,
                "/fixture-bucket/root/other".into(),
                request_received,
                true,
            )
        } else {
            (client, Arc::new(AtomicUsize::new(0)))
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scratch");
        let read = client
            .get_to_file(&json!({"Key":"root/data","VersionId":"read"}), &path)
            .unwrap();
        assert!(read.body.is_empty());
        assert_eq!(fs::read(&path).unwrap(), bytes);
        let written = client
            .put_file(&json!({"Key":"root/other","IfNoneMatch":"*"}), &path)
            .unwrap();
        assert_eq!(written.value["VersionId"], "written");
        assert_eq!(injected.load(Ordering::SeqCst), usize::from(interrupted));
        let requests = worker.finish_requests();
        let uploads = requests
            .iter()
            .filter(|request| request.method == "PUT")
            .collect::<Vec<_>>();
        assert_eq!(uploads.len(), 1);
        let upload = uploads[0];
        assert_eq!(upload.body, bytes);
        assert_eq!(upload.headers["content-length"], bytes.len().to_string());
        assert_eq!(
            upload.headers["x-amz-content-sha256"],
            encode_lower(Sha256::digest(&bytes))
        );
    }

    #[test]
    fn interrupted_read_connections_recover_exact_metadata_and_bytes() {
        let bytes = vec![0, 255, 42, 13, 10];
        for (operation, streamed, failures) in [
            ("head_object", false, 1),
            ("get_object", false, 1),
            ("get_object", true, 2),
        ] {
            let (client, worker) = single_reply_fixture(Reply {
                status: 200,
                headers: vec![
                    ("Content-Length", bytes.len().to_string()),
                    ("x-amz-version-id", "v/+=1".into()),
                    ("ETag", "\"abc\"".into()),
                    ("x-amz-meta-proof", "exact".into()),
                ],
                body: if operation == "head_object" {
                    Vec::new()
                } else {
                    bytes.clone()
                },
            });
            let (client, attempts) = inject_connector_failure(
                client,
                failures,
                InjectedFailure::Io(std::io::ErrorKind::Interrupted),
            );
            let args = json!({"Key":"root/a//b","VersionId":"v/+=1","IfMatch":"\"abc\""});
            let directory = tempfile::tempdir().unwrap();
            let destination = directory.path().join("scratch");
            let response = if streamed {
                client.get_to_file(&args, &destination).unwrap()
            } else {
                client.call_s3(operation, &args, None).unwrap()
            };
            assert!((failures + 1..=3).contains(&attempts.load(Ordering::SeqCst)));
            assert_eq!(response.value["ContentLength"], bytes.len());
            assert_eq!(response.value["VersionId"], "v/+=1");
            assert_eq!(response.value["ETag"], "\"abc\"");
            assert_eq!(response.value["Metadata"]["proof"], "exact");
            if streamed {
                assert_eq!(fs::read(destination).unwrap(), bytes);
                assert!(response.body.is_empty());
            } else if operation == "get_object" {
                assert_eq!(response.body, bytes);
            } else {
                assert!(response.body.is_empty());
            }
            let requests = worker.finish_requests();
            assert!(!requests.is_empty());
            for request in &requests {
                assert_eq!(
                    request.method,
                    if operation == "head_object" {
                        "HEAD"
                    } else {
                        "GET"
                    }
                );
                assert_eq!(
                    request.target,
                    "/fixture-bucket/root/a//b?versionId=v%2F%2B%3D1"
                );
                assert_eq!(request.headers["if-match"], "\"abc\"");
                assert!(request.headers["authorization"].contains("if-match"));
            }
        }
    }

    #[test]
    fn interrupted_read_connections_are_bounded_to_three_attempts() {
        for (operation, streamed) in [
            ("head_object", false),
            ("get_object", false),
            ("get_object", true),
        ] {
            let (client, worker) = empty_fixture();
            let (client, attempts) = inject_connector_failure(
                client,
                usize::MAX,
                InjectedFailure::Io(std::io::ErrorKind::Interrupted),
            );
            let args = json!({"Key":"root/a","VersionId":"v1"});
            let directory = tempfile::tempdir().unwrap();
            let destination = directory.path().join("scratch");
            let error = if streamed {
                client.get_to_file(&args, &destination).unwrap_err()
            } else {
                client.call_s3(operation, &args, None).unwrap_err()
            };
            assert_eq!(error.code, "TransportError");
            assert!(error.message.contains("injected connection failure"));
            assert_eq!(attempts.load(Ordering::SeqCst), 3);
            assert!(!destination.exists());
            assert!(worker.finish_requests().is_empty());
        }
    }

    #[test]
    fn read_connections_do_not_retry_other_io_errors_or_timeouts() {
        for failure in [
            InjectedFailure::Io(std::io::ErrorKind::PermissionDenied),
            InjectedFailure::Io(std::io::ErrorKind::TimedOut),
            InjectedFailure::Io(std::io::ErrorKind::ConnectionReset),
            InjectedFailure::Timeout,
        ] {
            let (client, worker) = empty_fixture();
            let (client, attempts) = inject_connector_failure(client, usize::MAX, failure);
            let error = client
                .call_s3(
                    "get_object",
                    &json!({"Key":"root/a","VersionId":"v1"}),
                    None,
                )
                .unwrap_err();
            assert_eq!(error.code, "TransportError");
            assert_eq!(attempts.load(Ordering::SeqCst), 1);
            assert!(worker.finish_requests().is_empty());
        }
    }

    #[test]
    fn mutation_responses_resume_on_the_same_connection_without_replay() {
        for (operation, method, target, response_body) in [
            ("put_object", "PUT", "/fixture-bucket/root/a", ""),
            (
                "delete_object",
                "DELETE",
                "/fixture-bucket/root/a?versionId=v1",
                "",
            ),
            (
                "create_multipart_upload",
                "POST",
                "/fixture-bucket/root/a?uploads=",
                "<InitiateMultipartUploadResult><UploadId>upload</UploadId></InitiateMultipartUploadResult>",
            ),
            (
                "complete_multipart_upload",
                "POST",
                "/fixture-bucket/root/a?uploadId=upload",
                "<CompleteMultipartUploadResult><ETag>complete</ETag></CompleteMultipartUploadResult>",
            ),
        ] {
            let (sent, received) = mpsc::channel();
            let (client, worker) = routed_fixture(move |request| {
                assert_eq!(request.method, method);
                assert_eq!(request.target, target);
                sent.send(()).unwrap();
                Reply {
                    status: 200,
                    headers: vec![("x-amz-version-id", "v1".into())],
                    body: response_body.as_bytes().to_vec(),
                }
            });
            let (client, injected) =
                inject_response_interruption(client, target.into(), received, true);
            let mut args = json!({"Key":"root/a"});
            if operation == "delete_object" {
                args["VersionId"] = "v1".into();
            }
            if operation == "complete_multipart_upload" {
                args["UploadId"] = "upload".into();
                args["MultipartUpload"] = json!({"Parts":[{"PartNumber":1,"ETag":"\"part\""}]});
            }
            let response = client.call_s3(operation, &args, Some(b"abc")).unwrap();
            assert_eq!(response.value["VersionId"], "v1", "{operation}");
            assert_eq!(injected.load(Ordering::SeqCst), 1, "{operation}");
            let requests = worker.finish_requests();
            assert_eq!(requests.len(), 1, "{operation}");
            if operation == "put_object" {
                assert_eq!(requests[0].body, b"abc");
            }
            if operation == "create_multipart_upload" {
                assert_eq!(response.value["UploadId"], "upload");
            }
            if operation == "complete_multipart_upload" {
                assert_eq!(response.value["ETag"], "complete");
                assert!(String::from_utf8_lossy(&requests[0].body).contains("&quot;part&quot;"));
            }
        }
    }

    #[test]
    fn interrupted_mutation_connections_are_never_replayed() {
        let args = json!({"Key":"root/a","VersionId":"v1","CopySource":{"Bucket":"fixture-bucket","Key":"root/source","VersionId":"src"},"UploadId":"upload","PartNumber":1,"MultipartUpload":{"Parts":[{"PartNumber":1,"ETag":"\"part\""}]}});
        for operation in [
            "put_object",
            "copy_object",
            "delete_object",
            "delete_object_tagging",
            "create_multipart_upload",
            "upload_part",
            "upload_part_copy",
            "complete_multipart_upload",
            "abort_multipart_upload",
        ] {
            let (client, worker) = empty_fixture();
            let (client, attempts) = inject_connector_failure(
                client,
                usize::MAX,
                InjectedFailure::Io(std::io::ErrorKind::Interrupted),
            );
            let error = client.call_s3(operation, &args, Some(b"abc")).unwrap_err();
            assert_eq!(error.code, "TransportError", "{operation}");
            assert_eq!(attempts.load(Ordering::SeqCst), 1, "{operation}");
            assert!(worker.finish_requests().is_empty());
        }
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        fs::write(&source, b"abc").unwrap();
        for part in [false, true] {
            let (client, worker) = empty_fixture();
            let (client, attempts) = inject_connector_failure(
                client,
                usize::MAX,
                InjectedFailure::Io(std::io::ErrorKind::Interrupted),
            );
            let error = if part {
                client.upload_part_file(&args, &source, 0, 3).unwrap_err()
            } else {
                client.put_file(&args, &source).unwrap_err()
            };
            assert_eq!(error.code, "TransportError");
            assert_eq!(attempts.load(Ordering::SeqCst), 1);
            assert!(worker.finish_requests().is_empty());
        }
    }

    #[test]
    fn streaming_upload_part_sends_only_selected_file_range() {
        let (client, worker) = single_reply_fixture(Reply {
            status: 200,
            headers: vec![("ETag", "\"part\"".into())],
            body: Vec::new(),
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source");
        fs::write(&path, b"before-PART-after").unwrap();
        let response = client
            .upload_part_file(
                &json!({"Key":"root/a","UploadId":"upload","PartNumber":2}),
                &path,
                7,
                4,
            )
            .unwrap();
        assert_eq!(response.value["ETag"], "\"part\"");
        let requests = worker.finish_requests();
        assert_eq!(requests[0].body, b"PART");
        assert_eq!(requests[0].headers["content-length"], "4");
        assert!(!requests[0].headers.contains_key("transfer-encoding"));
    }

    #[test]
    fn copy_preserves_exact_source_metadata_and_properties() {
        let(client,worker)=single_reply_fixture(Reply{status:200,headers:vec![("x-amz-version-id","dst".into())],body:b"<CopyObjectResult><ETag>&quot;new&quot;</ETag><LastModified>2026-10-07T03:04:05.123Z</LastModified></CopyObjectResult>".to_vec()});
        let response=client.call_s3("copy_object",&json!({"Key":"root/archive/task/a","CopySource":{"Bucket":"fixture-bucket","Key":"root/task/a ?%","VersionId":"src/+="},"CopySourceIfMatch":"\"old\"","MetadataDirective":"REPLACE","TaggingDirective":"COPY","Metadata":{"owner":"tx"},"ContentType":"text/plain","CacheControl":"max-age=7","BucketKeyEnabled":true,"ObjectLockMode":"GOVERNANCE","ObjectLockLegalHoldStatus":"ON"}),None).unwrap();
        assert_eq!(response.value["CopyObjectResult"]["ETag"], "\"new\"");
        assert_eq!(
            response.value["CopyObjectResult"]["LastModified"],
            "2026-10-07T03:04:05.123000+00:00"
        );
        let request = worker.finish_requests().pop().unwrap();
        assert_eq!(
            request.headers["x-amz-copy-source"],
            "/fixture-bucket/root/task/a%20%3F%25?versionId=src%2F%2B%3D"
        );
        assert_eq!(request.headers["x-amz-copy-source-if-match"], "\"old\"");
        assert_eq!(request.headers["x-amz-meta-owner"], "tx");
        assert_eq!(request.headers["x-amz-tagging-directive"], "COPY");
        assert_eq!(
            request.headers["x-amz-server-side-encryption-bucket-key-enabled"],
            "true"
        );
    }

    #[test]
    fn complete_multipart_embedded_200_error_is_not_success() {
        let (client, worker) = single_reply_fixture(Reply::xml(
            "<Error><Code>InternalError</Code><Message>completion failed</Message></Error>",
        ));
        let error=client.call_s3("complete_multipart_upload",&json!({"Key":"root/a","UploadId":"id","MultipartUpload":{"Parts":[{"PartNumber":1,"ETag":"\"part&amp;\""}]}}),None).unwrap_err();
        assert_eq!(error.code, "InternalError");
        assert_eq!(error.status, Some(200));
        assert!(error.is_retryable());
        let request = worker.finish_requests().pop().unwrap();
        assert_eq!(request.target, "/fixture-bucket/root/a?uploadId=id");
        assert!(
            String::from_utf8(request.body)
                .unwrap()
                .contains("&quot;part&amp;amp;&quot;")
        );
    }

    #[test]
    fn provider_code_and_rejected_header_are_preserved_without_retry() {
        let(client,worker)=single_reply_fixture(Reply{status:501,headers:Vec::new(),body:b"<Error><Code>NotImplemented</Code><Message>A header implies unimplemented functionality</Message><Header>If-None-Match</Header></Error>".to_vec()});
        let error = client
            .call_s3(
                "put_object",
                &json!({"Key":"root/receipt","IfNoneMatch":"*"}),
                Some(b"{}"),
            )
            .unwrap_err();
        assert_eq!(error.code, "NotImplemented");
        assert_eq!(error.header.as_deref(), Some("If-None-Match"));
        assert_eq!(worker.finish_requests().len(), 1);
    }

    #[test]
    fn named_404_or_403_errors_are_never_missing_object_versions() {
        for status in [403, 404] {
            let (client, worker) = single_reply_fixture(Reply {
                status,
                headers: Vec::new(),
                body:
                    b"<Error><Code>NoSuchBucket</Code><Message>bucket unavailable</Message></Error>"
                        .to_vec(),
            });
            let error = client
                .call_s3(
                    "get_object",
                    &json!({"Key":"root/a","VersionId":"v1"}),
                    None,
                )
                .unwrap_err();
            assert!(
                !error.is_missing(),
                "NoSuchBucket must not trigger archive alias lookup"
            );
            worker.finish_requests();
        }
        let (client, worker) = single_reply_fixture(Reply {
            status: 404,
            headers: Vec::new(),
            body: b"<Error><Code>NoSuchVersion</Code><Message>version absent</Message></Error>"
                .to_vec(),
        });
        assert!(
            client
                .call_s3(
                    "get_object",
                    &json!({"Key":"root/a","VersionId":"v1"}),
                    None
                )
                .unwrap_err()
                .is_missing()
        );
        worker.finish_requests();
    }

    fn history_page(rows: &str, tail: &str) -> Reply {
        Reply::xml(&format!(
            "<ListVersionsResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">{rows}{tail}</ListVersionsResult>"
        ))
    }

    fn history_fixture(
        prefix: &'static str,
        first: Reply,
        next: Option<(&'static str, &'static str, Reply)>,
    ) -> (S3Client, RoutedFixture) {
        let first = Arc::new(first);
        let next = next.map(|(key, version, reply)| (key, version, Arc::new(reply)));
        replayable_read_fixture(move |request| {
            assert_eq!(request.method, "GET");
            let url = url::Url::parse(&format!("http://fixture{}", request.target)).unwrap();
            assert_eq!(url.path(), "/fixture-bucket");
            let query = url.query_pairs().into_owned().collect::<BTreeMap<_, _>>();
            let mut expected = BTreeMap::from([
                ("versions".to_owned(), String::new()),
                ("max-keys".to_owned(), "1000".to_owned()),
                ("prefix".to_owned(), prefix.to_owned()),
            ]);
            if let Some(key) = query.get("key-marker") {
                let (next_key, next_version, reply) = next.as_ref().expect("unexpected pagination");
                assert_eq!(key, next_key);
                expected.insert("key-marker".into(), (*next_key).into());
                expected.insert("version-id-marker".into(), (*next_version).into());
                assert_eq!(query, expected);
                reply.clone()
            } else {
                assert_eq!(query, expected);
                first.clone()
            }
        })
    }
    const VERSION: &str = "<Version><Key>root/task/a&amp;b</Key><VersionId>v1</VersionId><IsLatest>false</IsLatest><LastModified>2026-10-07T00:00:00.7Z</LastModified><ETag>&quot;abc&quot;</ETag><Size>5</Size></Version>";
    const MARKER: &str = "<DeleteMarker><Key>root/task/a&amp;b</Key><VersionId>d1</VersionId><IsLatest>true</IsLatest><LastModified>2026-10-07T00:00:01Z</LastModified></DeleteMarker>";

    #[test]
    fn complete_version_pagination_keeps_all_markers() {
        let (client, worker) = history_fixture(
            "root/task/",
            history_page(
                VERSION,
                "<IsTruncated>true</IsTruncated><NextKeyMarker>root/task/a&amp;b</NextKeyMarker><NextVersionIdMarker>v/+=</NextVersionIdMarker>",
            ),
            Some((
                "root/task/a&b",
                "v/+=",
                history_page(MARKER, "<IsTruncated>false</IsTruncated>"),
            )),
        );
        let rows = client.list_versions("root/task/").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["Key"], "root/task/a&b");
        assert_eq!(rows[0]["LastModified"], "2026-10-07T00:00:00.700000+00:00");
        assert_eq!(rows[0]["delete_marker"], false);
        assert_eq!(rows[1]["delete_marker"], true);
        assert_eq!(rows[1]["VersionId"], "d1");
        let requests = worker.finish_requests();
        let next = requests
            .iter()
            .find(|request| request.target.contains("key-marker="))
            .unwrap();
        assert!(next.target.contains("key-marker=root%2Ftask%2Fa%26b"));
        assert!(next.target.contains("version-id-marker=v%2F%2B%3D"));
    }

    #[test]
    fn history_fails_closed_on_repeated_identity_or_marker() {
        let tail = "<IsTruncated>true</IsTruncated><NextKeyMarker>root/task/a&amp;b</NextKeyMarker><NextVersionIdMarker>v1</NextVersionIdMarker>";
        let (client, worker) = history_fixture(
            "root/task/",
            history_page(VERSION, tail),
            Some(("root/task/a&b", "v1", history_page(VERSION, tail))),
        );
        assert!(
            client
                .list_versions("root/task/")
                .unwrap_err()
                .to_string()
                .contains("repeated an object version")
        );
        worker.finish_requests();
        let (client, worker) = history_fixture(
            "root/task/",
            history_page("", tail),
            Some(("root/task/a&b", "v1", history_page("", tail))),
        );
        assert!(
            client
                .list_versions("root/task/")
                .unwrap_err()
                .to_string()
                .contains("repeated pagination")
        );
        worker.finish_requests();
    }

    #[test]
    fn history_fails_closed_on_escaped_key_and_missing_pagination() {
        let (client, worker) = history_fixture(
            "root/other/",
            history_page(VERSION, "<IsTruncated>false</IsTruncated>"),
            None,
        );
        assert!(
            client
                .list_versions("root/other/")
                .unwrap_err()
                .to_string()
                .contains("escaped")
        );
        worker.finish_requests();
        let (client, worker) = history_fixture(
            "root/task/",
            history_page(
                "",
                "<IsTruncated>true</IsTruncated><NextKeyMarker>root/task/a</NextKeyMarker>",
            ),
            None,
        );
        assert!(
            client
                .list_versions("root/task/")
                .unwrap_err()
                .to_string()
                .contains("NextVersionIdMarker")
        );
        worker.finish_requests();
    }

    #[test]
    fn xml_listing_preserves_namespaced_elements_and_key_text() {
        let node = parse_xml(
            "<s3:ListBucketResult xmlns:s3=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
             <s3:IsTruncated>false</s3:IsTruncated><s3:KeyCount>2</s3:KeyCount><s3:Contents>\
             <s3:Key> 目录/&amp;&lt;&gt;&quot;&apos;&amp;lt;&#65;&#x4E2D;&#x1F600;\
             <![CDATA[/raw&<>&amp;\r\n]]>tail </s3:Key>\
             <s3:Size>7</s3:Size><s3:StorageClass/></s3:Contents>\
             <s3:Contents><s3:Key>second</s3:Key><s3:Size>0</s3:Size></s3:Contents>\
             <s3:CommonPrefixes><s3:Prefix>目录/</s3:Prefix></s3:CommonPrefixes>\
             <s3:CommonPrefixes><s3:Prefix>other/</s3:Prefix></s3:CommonPrefixes>\
             </s3:ListBucketResult>"
                .as_bytes(),
        )
        .unwrap();
        let result = operation_result("list_objects_v2", &node).unwrap();
        assert_eq!(result["IsTruncated"], false);
        assert_eq!(result["KeyCount"], 2);
        assert_eq!(result["Contents"].as_array().unwrap().len(), 2);
        assert_eq!(
            result["Contents"][0]["Key"],
            " 目录/&<>\"'&lt;A中😀/raw&<>&amp;\r\ntail "
        );
        assert_eq!(result["Contents"][0]["Size"], 7);
        assert_eq!(result["Contents"][0]["StorageClass"], "");
        assert_eq!(result["Contents"][1], json!({"Key": "second", "Size": 0}));
        assert_eq!(
            result["CommonPrefixes"],
            json!([{"Prefix": "目录/"}, {"Prefix": "other/"}])
        );
    }

    #[test]
    fn xml_empty_object_listing_retains_contents_array() {
        let node =
            parse_xml(b"<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>")
                .unwrap();
        let result = operation_result("list_objects_v2", &node).unwrap();
        assert_eq!(result["Contents"], json!([]));
        assert!(result.get("CommonPrefixes").is_none());
    }

    #[test]
    fn xml_text_preserves_line_endings_and_whitespace() {
        let node = parse_xml(b"<Root><Key> \r\na\rb\nc\t </Key></Root>").unwrap();
        assert_eq!(node.child("Key").unwrap().text, " \r\na\rb\nc\t ");
    }

    #[test]
    fn xml_invalid_utf8_and_entities_fail() {
        for raw in [
            b"<Ro\xffot/>".as_slice(),
            b"<Root>\xff</Root>".as_slice(),
            b"<Root><![CDATA[\xff]]></Root>".as_slice(),
            b"<Root>&bad\xff;</Root>".as_slice(),
            b"<Root>&unknown;</Root>".as_slice(),
            b"<Root>&#x110000;</Root>".as_slice(),
            b"<Root>&#not-a-number;</Root>".as_slice(),
        ] {
            assert!(parse_xml(raw).is_err(), "accepted invalid XML: {raw:?}");
        }
    }

    #[test]
    fn malformed_xml_boolean_and_truncated_response_fail() {
        assert!(parse_xml(b"<Root><Key>x</Root>").is_err());
        assert!(parse_xml(b"<!DOCTYPE Root><Root/>").is_err());
        let node = parse_xml(
            b"<ListVersionsResult><IsTruncated>unknown</IsTruncated></ListVersionsResult>",
        )
        .unwrap();
        assert!(operation_result("list_object_versions", &node).is_err());
        let node = parse_xml(b"<ListVersionsResult/>").unwrap();
        assert!(operation_result("list_object_versions", &node).is_err());
    }

    #[test]
    fn request_mutations_never_redirect() {
        let (client, worker) = single_reply_fixture(Reply {
            status: 307,
            headers: vec![("Location", "http://127.0.0.1:1/never-follow".into())],
            body: Vec::new(),
        });
        assert_eq!(
            client
                .call_s3(
                    "put_object",
                    &json!({"Key":"root/a"}),
                    Some(b"secret-free-fixture")
                )
                .unwrap_err()
                .status,
            Some(307)
        );
        assert_eq!(worker.finish_requests().len(), 1);
    }

    #[test]
    fn head_and_exact_delete_preserve_version_id() {
        let (client, worker) =
            routed_fixture(
                |request| match (request.method.as_str(), request.target.as_str()) {
                    ("HEAD", "/fixture-bucket/root/a?versionId=v1") => Reply {
                        status: 200,
                        headers: vec![
                            ("Content-Length", "55".into()),
                            ("x-amz-version-id", "v1".into()),
                            ("x-amz-meta-user", "m".into()),
                            ("Content-Type", "text/plain".into()),
                        ],
                        body: Vec::new(),
                    },
                    ("DELETE", "/fixture-bucket/root/a?versionId=d1") => Reply {
                        status: 204,
                        headers: vec![
                            ("x-amz-version-id", "d1".into()),
                            ("x-amz-delete-marker", "true".into()),
                        ],
                        body: Vec::new(),
                    },
                    _ => panic!("unexpected exact head/delete request: {request:?}"),
                },
            );
        let head = client
            .call_s3(
                "head_object",
                &json!({"Key":"root/a","VersionId":"v1"}),
                None,
            )
            .unwrap();
        assert_eq!(head.value["ContentLength"], 55);
        assert_eq!(head.value["ContentType"], "text/plain");
        let deleted = client
            .call_s3(
                "delete_object",
                &json!({"Key":"root/a","VersionId":"d1"}),
                None,
            )
            .unwrap();
        assert_eq!(deleted.value["DeleteMarker"], true);
        let requests = worker.finish_requests();
        assert!(requests.iter().any(|request| request.method == "HEAD"));
        let deletes = requests
            .iter()
            .filter(|request| request.method == "DELETE")
            .collect::<Vec<_>>();
        assert_eq!(deletes.len(), 1);
        assert!(deletes[0].target.ends_with("versionId=d1"));
    }

    #[test]
    fn tagging_and_multipart_apis_encode_and_parse_native_requests() {
        let (client, worker) = routed_fixture(|request| {
            match (request.method.as_str(), request.target.as_str()) {
                ("GET", "/fixture-bucket/root/a?tagging=&versionId=src") => Reply::xml(
                    "<Tagging><TagSet><Tag><Key>a&amp;b</Key><Value>v+1</Value></Tag></TagSet></Tagging>",
                ),
                ("POST", "/fixture-bucket/root/a?uploads=") => Reply::xml(
                    "<InitiateMultipartUploadResult><Bucket>fixture-bucket</Bucket><Key>root/a</Key><UploadId>upload/+</UploadId></InitiateMultipartUploadResult>",
                ),
                ("PUT", "/fixture-bucket/root/a?partNumber=1&uploadId=upload%2F%2B") => Reply::xml(
                    "<CopyPartResult><ETag>&quot;part&quot;</ETag><LastModified>2026-10-07T00:00:00Z</LastModified></CopyPartResult>",
                ),
                ("POST", "/fixture-bucket/root/a?uploadId=upload%2F%2B") => Reply::xml(
                    "<CompleteMultipartUploadResult><ETag>&quot;total-1&quot;</ETag><Key>root/a</Key></CompleteMultipartUploadResult>",
                ),
                ("DELETE", "/fixture-bucket/root/a?uploadId=upload%2F%2B") => Reply {
                    status: 204,
                    headers: Vec::new(),
                    body: Vec::new(),
                },
                _ => panic!("unexpected tagging/multipart request: {request:?}"),
            }
        });
        let tags = client
            .call_s3(
                "get_object_tagging",
                &json!({"Key":"root/a","VersionId":"src"}),
                None,
            )
            .unwrap();
        assert_eq!(tags.value["TagSet"][0]["Key"], "a&b");
        let created = client
            .call_s3(
                "create_multipart_upload",
                &json!({"Key":"root/a","Metadata":{"token":"tx"},"Tagging":"a%26b=v%2B1"}),
                None,
            )
            .unwrap();
        assert_eq!(created.value["UploadId"], "upload/+");
        let part=client.call_s3("upload_part_copy",&json!({"Key":"root/a","UploadId":"upload/+","PartNumber":1,"CopySource":{"Bucket":"fixture-bucket","Key":"root/src","VersionId":"src"},"CopySourceRange":"bytes=0-511","CopySourceIfMatch":"\"old\""}),None).unwrap();
        assert_eq!(part.value["CopyPartResult"]["ETag"], "\"part\"");
        let done=client.call_s3("complete_multipart_upload",&json!({"Key":"root/a","UploadId":"upload/+","MultipartUpload":{"Parts":[{"PartNumber":1,"ETag":"\"part\""}]}}),None).unwrap();
        assert_eq!(done.value["ETag"], "\"total-1\"");
        client
            .call_s3(
                "abort_multipart_upload",
                &json!({"Key":"root/a","UploadId":"upload/+"}),
                None,
            )
            .unwrap();
        let requests = worker.finish_requests();
        let mutations = requests
            .iter()
            .filter(|request| request.method != "GET")
            .collect::<Vec<_>>();
        assert_eq!(mutations.len(), 4);
        let part = mutations
            .iter()
            .find(|request| request.method == "PUT")
            .unwrap();
        assert_eq!(part.headers["x-amz-copy-source-range"], "bytes=0-511");
        assert!(part.target.contains("partNumber=1&uploadId=upload%2F%2B"));
        assert_eq!(mutations[3].method, "DELETE");
    }

    #[test]
    fn endpoint_and_b2_region_detection_do_not_guess_custom_aliases() {
        let b2 = client("https://s3.us-west-004.backblazeb2.com");
        assert!(b2.b2);
        assert_eq!(
            b2_region("https://s3.us-west-004.backblazeb2.com").as_deref(),
            Some("us-west-004")
        );
        assert!(!client("https://b2.example.invalid").b2);
        assert!(
            S3Client::new(
                "b",
                "p",
                "https://u:p@example.invalid",
                "us-east-1",
                test_credentials()
            )
            .is_err()
        );
        assert!(
            S3Client::new(
                "b",
                "p",
                "https://example.invalid?x=1",
                "us-east-1",
                test_credentials()
            )
            .is_err()
        );
    }

    #[test]
    fn remote_ini_preserves_outer_quotes_without_stripping_remote_name() {
        let ini = "[core]\nremote = workspace-mgr\n['remote \"workspace-mgr\"']\n access_key_id = fixture\n secret_access_key = opaque\n";
        let remote = ini_section(ini, "remote \"workspace-mgr\"");
        assert_eq!(remote["access_key_id"], "fixture");
        assert_eq!(remote["secret_access_key"], "opaque");
        assert_eq!(
            ini_section("[profile fixture]\nregion = us-west-2\n", "profile fixture")["region"],
            "us-west-2"
        );
    }

    #[test]
    fn native_repository_uses_tracked_routing_and_ignores_legacy_remote_files() {
        let directory = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: directory.path().to_owned(),
        };
        configure_repo(&client("http://127.0.0.1:1"), &repo);
        fs::create_dir(repo.root.join(".dvc")).unwrap();
        fs::write(repo.root.join(".dvc/config"), "[core]\nremote = other\n['remote \"other\"']\nurl = s3://wrong/wrong\nendpointurl = https://wrong.invalid\n").unwrap();
        fs::write(
            repo.root.join(".dvc/config.local"),
            "this is not valid configuration\n",
        )
        .unwrap();
        let resolved = S3Client::from_repo(&repo).unwrap();
        assert_eq!(resolved.bucket, "fixture-bucket");
        assert_eq!(resolved.prefix, "root");
        assert_eq!(resolved.endpoint, "http://127.0.0.1:1");
        assert_eq!(resolved.credentials.access, "fixture-test-only");
    }

    #[test]
    fn malformed_root_configuration_never_falls_back_to_a_legacy_remote() {
        let directory = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: directory.path().to_owned(),
        };
        repo.run(["init", "-q"]).unwrap();
        fs::create_dir(repo.root.join(".dvc")).unwrap();
        fs::write(repo.root.join(".dvc/config"), "[core]\nremote = old\n['remote \"old\"']\nurl = s3://legacy/root\naccess_key_id = fixture\nsecret_access_key = fixture-secret\n").unwrap();
        fs::write(Config::path(&repo), "not valid TOML [").unwrap();
        assert!(
            S3Client::from_repo(&repo)
                .err()
                .unwrap()
                .to_string()
                .contains(".workspace-mgr.toml")
        );
        fs::remove_file(Config::path(&repo)).unwrap();
        let resolved = S3Client::from_repo(&repo).unwrap();
        assert_eq!(resolved.bucket, "legacy");
        assert_eq!(resolved.credentials.access, "fixture");
    }

    #[test]
    fn local_credentials_reject_routing_overrides_and_redact_parse_errors() {
        let directory = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: directory.path().to_owned(),
        };
        repo.run(["init", "-q"]).unwrap();
        fs::create_dir_all(repo.root.join(".workspace-mgr/local")).unwrap();
        for raw in [
            "endpoint_url = \"https://routing-secret-should-not-print.invalid\"\n",
            "access_key_id = \"credential-secret-should-not-print\"\nsecret_access_key = 123\n",
            "secret_access_key = \"syntax-secret-should-not-print\n",
        ] {
            fs::write(repo.root.join(CREDENTIALS_NAME), raw).unwrap();
            let message = load_credentials_config(&repo).err().unwrap().to_string();
            assert!(message.contains("invalid local S3 credentials configuration"));
            assert!(!message.contains("secret-should-not-print"));
        }
    }

    #[test]
    fn linked_worktrees_share_credentials_from_the_primary_checkout() {
        let directory = tempfile::tempdir().unwrap();
        let primary_root = directory.path().join("primary");
        fs::create_dir(&primary_root).unwrap();
        let primary = GitRepo { root: primary_root };
        configure_repo(&client("http://127.0.0.1:1"), &primary);
        primary.run(["add", ".workspace-mgr.toml"]).unwrap();
        primary
            .run([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "Native configuration",
            ])
            .unwrap();
        let linked_root = directory.path().join("linked");
        primary
            .run(["worktree", "add", "--detach", linked_root.to_str().unwrap()])
            .unwrap();
        let linked = GitRepo { root: linked_root };
        fs::create_dir_all(linked.root.join(".workspace-mgr/local")).unwrap();
        fs::write(
            linked.root.join(CREDENTIALS_NAME),
            "access_key_id = \"wrong-worktree\"\nsecret_access_key = \"wrong-worktree-secret\"\n",
        )
        .unwrap();
        assert_eq!(
            credentials_path(&linked).unwrap().canonicalize().unwrap(),
            primary.root.join(CREDENTIALS_NAME).canonicalize().unwrap()
        );
        assert_eq!(
            load_credentials_config(&linked).unwrap().access_key_id,
            Some("fixture-test-only".to_owned())
        );
        assert_eq!(
            S3Client::from_repo(&linked).unwrap().credentials.access,
            "fixture-test-only"
        );
    }

    #[test]
    fn historical_worktrees_keep_legacy_routing_and_use_migrated_primary_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let primary_root = directory.path().join("primary");
        fs::create_dir(&primary_root).unwrap();
        let primary = GitRepo { root: primary_root };
        primary.run(["init", "-q"]).unwrap();
        fs::create_dir(primary.root.join(".dvc")).unwrap();
        fs::write(
            primary.root.join(".dvc/config"),
            "[core]\nremote = old\n['remote \"old\"']\nurl = s3://historical-bucket/old-prefix\nendpointurl = http://127.0.0.1:2\naccess_key_id = historical-credential\nsecret_access_key = historical-secret\n",
        )
        .unwrap();
        primary.run(["add", ".dvc/config"]).unwrap();
        primary
            .run([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "Historical remote configuration",
            ])
            .unwrap();
        configure_repo(&client("http://127.0.0.1:1"), &primary);
        let historical_root = directory.path().join("historical");
        primary
            .run([
                "worktree",
                "add",
                "--detach",
                historical_root.to_str().unwrap(),
            ])
            .unwrap();
        let historical = GitRepo {
            root: historical_root,
        };
        assert!(!Config::path(&historical).exists());
        let resolved = S3Client::from_repo(&historical).unwrap();
        assert_eq!(resolved.bucket, "historical-bucket");
        assert_eq!(resolved.prefix, "old-prefix");
        assert_eq!(resolved.endpoint, "http://127.0.0.1:2");
        assert_eq!(resolved.credentials.access, "fixture-test-only");

        fs::remove_file(primary.root.join(CREDENTIALS_NAME)).unwrap();
        let fallback = S3Client::from_repo(&historical).unwrap();
        assert_eq!(fallback.bucket, "historical-bucket");
        assert_eq!(fallback.credentials.access, "historical-credential");
    }

    #[cfg(unix)]
    #[test]
    fn local_credentials_cannot_traverse_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let repo = GitRepo {
            root: directory.path().to_owned(),
        };
        repo.run(["init", "-q"]).unwrap();
        fs::create_dir_all(repo.root.join(".workspace-mgr/local")).unwrap();
        let outside = directory.path().join("private-source");
        fs::write(
            &outside,
            "access_key_id = \"fixture\"\nsecret_access_key = \"fixture-secret\"\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(&outside, repo.root.join(CREDENTIALS_NAME)).unwrap();
        assert!(
            load_credentials_config(&repo)
                .err()
                .unwrap()
                .to_string()
                .contains("may not traverse a symlink")
        );
    }

    #[test]
    fn partial_explicit_credentials_do_not_fall_back() {
        let remote = BTreeMap::from([("access_key_id".to_owned(), "fixture".to_owned())]);
        let saved = BTreeMap::from([
            ("aws_access_key_id".to_owned(), "other".to_owned()),
            (
                "aws_secret_access_key".to_owned(),
                "other-secret".to_owned(),
            ),
        ]);
        assert!(
            resolve_credentials(&remote, &saved, &BTreeMap::new())
                .err()
                .unwrap()
                .to_string()
                .contains("incomplete")
        );
    }

    #[test]
    fn verified_file_checksum_and_size_mismatch_never_send_a_request() {
        let client = client("http://127.0.0.1:1");
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cache");
        fs::write(&path, b"abc").unwrap();
        for args in [
            json!({"Key":"root/a","ExpectedSize":4}),
            json!({"Key":"root/a","ExpectedMD5":"incorrect"}),
            json!({"Key":"root/a","ContentMD5":"incorrect"}),
        ] {
            let error = client.put_file(&args, &path).unwrap_err();
            assert_eq!(error.code, "InvalidRequest");
            assert!(error.message.contains("differs"));
        }
    }

    #[test]
    fn storage_prefix_keeps_literal_percent_unicode_and_slashes() {
        assert_eq!(
            storage_location("s3://bucket/prefix%20literal/é data//").unwrap(),
            ("bucket".to_owned(), "prefix%20literal/é data".to_owned())
        );
        assert_eq!(
            storage_location("s3://bucket//nested//prefix").unwrap(),
            ("bucket".to_owned(), "/nested//prefix".to_owned())
        );
        assert!(storage_location("s3://user:pass@bucket/prefix").is_err());
    }

    #[test]
    fn credential_process_tokenization_has_no_shell_expansion() {
        assert_eq!(
            command_words("program 'a b' \"c d\" literal\\ space $(never) $HOME").unwrap(),
            vec![
                "program",
                "a b",
                "c d",
                "literal space",
                "$(never)",
                "$HOME"
            ]
        );
        assert!(command_words("program 'unterminated").is_err());
        assert!(command_words("").is_err());
    }

    #[test]
    fn timestamps_match_historical_python_isoformat() {
        assert_eq!(
            normalize_timestamp("2026-10-07T00:00:00.123Z").unwrap(),
            "2026-10-07T00:00:00.123000+00:00"
        );
        assert_eq!(
            normalize_timestamp("2026-10-07T02:00:00.000000+02:00").unwrap(),
            "2026-10-07T00:00:00+00:00"
        );
        assert!(normalize_timestamp("2026-10-07T00:00:00").is_err());
    }
}
