use std::cmp::Ordering;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use semver::Version;
use serde::{Deserialize, Serialize};

const CACHE_SCHEMA: u32 = 1;
const CACHE_FILE: &str = "update-check.json";
const LOCK_FILE: &str = "update-check.lock";
const REGISTRY_URL: &str = "https://crates.io/api/v1/crates/workspace-mgr";
const SUCCESS_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const FAILURE_INTERVAL: Duration = Duration::from_secs(60 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_millis(750);
const RESPONSE_LIMIT: u64 = 1024 * 1024;
const CACHE_LIMIT: u64 = 64 * 1024;
const DISABLE_ENV: &str = "WORKSPACE_MGR_UPDATE_CHECK_DISABLE";

#[cfg(debug_assertions)]
const TEST_URL_ENV: &str = "WORKSPACE_MGR_UPDATE_TEST_URL";
#[cfg(debug_assertions)]
const TEST_CACHE_ENV: &str = "WORKSPACE_MGR_UPDATE_TEST_CACHE";

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct UpdateCache {
    schema_version: u32,
    last_attempt: Option<u64>,
    last_success: Option<u64>,
    latest_any: Option<String>,
    latest_stable: Option<String>,
}

impl UpdateCache {
    fn needs_refresh(&self, now: u64) -> bool {
        if self.schema_version != CACHE_SCHEMA {
            return true;
        }
        match (self.last_attempt, self.last_success) {
            (Some(attempt), success) if success.is_none_or(|value| attempt > value) => {
                !is_recent(now, attempt, FAILURE_INTERVAL)
            }
            (_, Some(success)) => !is_recent(now, success, SUCCESS_INTERVAL),
            _ => true,
        }
    }

    fn candidate_for(&self, current: &Version) -> Option<Version> {
        let candidate = if current.pre.is_empty() {
            self.latest_stable.as_deref()
        } else {
            self.latest_any.as_deref()
        }?;
        let candidate = Version::parse(candidate).ok()?;
        (candidate.cmp_precedence(current) == Ordering::Greater).then_some(candidate)
    }
}

#[derive(Debug, Clone)]
struct LatestVersions {
    any: Version,
    stable: Option<Version>,
}

#[derive(Debug, Deserialize)]
struct RegistryResponse {
    versions: Vec<RegistryVersion>,
}

#[derive(Debug, Deserialize)]
struct RegistryVersion {
    num: String,
    yanked: bool,
}

pub fn check_and_warn() {
    if std::env::var_os(DISABLE_ENV).is_some() {
        return;
    }
    let Some(cache_path) = cache_path() else {
        return;
    };
    let Ok(current) = Version::parse(env!("CARGO_PKG_VERSION")) else {
        return;
    };
    let endpoint = registry_url();
    let now = unix_seconds(SystemTime::now());
    if let Some(latest) = check(&cache_path, &current, now, || {
        fetch_registry(&endpoint, REQUEST_TIMEOUT)
    }) {
        eprintln!(
            "workspace-mgr: update available: {current} -> {latest}; agent: notify the user and ask before updating"
        );
    }
}

fn check<F>(cache_path: &Path, current: &Version, now: u64, fetch: F) -> Option<Version>
where
    F: FnOnce() -> io::Result<LatestVersions>,
{
    let initial = read_cache(cache_path).unwrap_or_default();
    if !initial.needs_refresh(now) {
        return initial.candidate_for(current);
    }

    let parent = cache_path.parent()?;
    if fs::create_dir_all(parent).is_err() {
        return initial.candidate_for(current);
    }
    let lock_path = parent.join(LOCK_FILE);
    let Ok(lock) = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
    else {
        return initial.candidate_for(current);
    };
    if lock.try_lock_exclusive().is_err() {
        return initial.candidate_for(current);
    }

    let current_cache = read_cache(cache_path).unwrap_or(initial);
    if !current_cache.needs_refresh(now) {
        return current_cache.candidate_for(current);
    }

    let mut updated = current_cache;
    updated.schema_version = CACHE_SCHEMA;
    updated.last_attempt = Some(now);
    if let Ok(latest) = fetch() {
        updated.last_success = Some(now);
        updated.latest_any = Some(latest.any.to_string());
        updated.latest_stable = latest.stable.map(|version| version.to_string());
    }
    let candidate = updated.candidate_for(current);
    let _ = write_cache(cache_path, &updated);
    candidate
}

fn fetch_registry(endpoint: &str, timeout: Duration) -> io::Result<LatestVersions> {
    let user_agent = format!(
        "workspace-mgr/{} (https://github.com/S-kblogN/workspace-mgr)",
        env!("CARGO_PKG_VERSION")
    );
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .timeout_connect(Some(timeout))
        .max_redirects(3)
        .build();
    let agent = ureq::Agent::new_with_config(config);
    let mut response = agent
        .get(endpoint)
        .header("User-Agent", &user_agent)
        .call()
        .map_err(io::Error::other)?;
    let body = response
        .body_mut()
        .with_config()
        .limit(RESPONSE_LIMIT)
        .read_to_vec()
        .map_err(io::Error::other)?;
    let payload: RegistryResponse = serde_json::from_slice(&body)?;

    let versions = payload
        .versions
        .into_iter()
        .filter(|entry| !entry.yanked)
        .filter_map(|entry| Version::parse(&entry.num).ok())
        .collect::<Vec<_>>();
    let any = versions
        .iter()
        .max_by(|left, right| left.cmp_precedence(right))
        .cloned()
        .ok_or_else(|| io::Error::other("registry response has no usable versions"))?;
    let stable = versions
        .into_iter()
        .filter(|version| version.pre.is_empty())
        .max_by(|left, right| left.cmp_precedence(right));
    Ok(LatestVersions { any, stable })
}

fn cache_path() -> Option<PathBuf> {
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os(TEST_CACHE_ENV).filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(path));
    }
    if let Some(root) = std::env::var_os("XDG_CACHE_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(root).join("workspace-mgr").join(CACHE_FILE));
    }
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)?;
    #[cfg(target_os = "macos")]
    let root = home.join("Library").join("Caches");
    #[cfg(not(target_os = "macos"))]
    let root = home.join(".cache");
    Some(root.join("workspace-mgr").join(CACHE_FILE))
}

fn registry_url() -> String {
    #[cfg(debug_assertions)]
    if let Some(url) = std::env::var_os(TEST_URL_ENV).filter(|value| !value.is_empty()) {
        return url.to_string_lossy().into_owned();
    }
    REGISTRY_URL.to_owned()
}

fn read_cache(path: &Path) -> Option<UpdateCache> {
    if fs::metadata(path).ok()?.len() > CACHE_LIMIT {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    let cache: UpdateCache = serde_json::from_slice(&bytes).ok()?;
    (cache.schema_version == CACHE_SCHEMA).then_some(cache)
}

fn write_cache(path: &Path, cache: &UpdateCache) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("update cache path has no parent"))?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut temporary, cache)?;
    temporary.write_all(b"\n")?;
    temporary.as_file_mut().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn unix_seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn is_recent(now: u64, timestamp: u64, interval: Duration) -> bool {
    timestamp <= now && now - timestamp < interval.as_secs()
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::thread;

    use super::*;

    fn cache(any: &str, stable: Option<&str>) -> UpdateCache {
        UpdateCache {
            schema_version: CACHE_SCHEMA,
            last_attempt: Some(100),
            last_success: Some(100),
            latest_any: Some(any.to_owned()),
            latest_stable: stable.map(str::to_owned),
        }
    }

    fn latest(any: &str, stable: Option<&str>) -> LatestVersions {
        LatestVersions {
            any: Version::parse(any).unwrap(),
            stable: stable.map(|value| Version::parse(value).unwrap()),
        }
    }

    #[test]
    fn prerelease_installations_follow_the_newest_channel() {
        let current = Version::parse("1.0.0-alpha.1").unwrap();
        assert_eq!(
            cache("1.1.0-beta.1", Some("1.0.0"))
                .candidate_for(&current)
                .unwrap(),
            Version::parse("1.1.0-beta.1").unwrap()
        );
    }

    #[test]
    fn stable_installations_ignore_newer_prereleases() {
        let current = Version::parse("1.0.0").unwrap();
        assert!(
            cache("1.1.0-beta.1", Some("1.0.0"))
                .candidate_for(&current)
                .is_none()
        );
        assert_eq!(
            cache("1.2.0-beta.1", Some("1.1.0"))
                .candidate_for(&current)
                .unwrap(),
            Version::parse("1.1.0").unwrap()
        );
    }

    #[test]
    fn fresh_cache_avoids_a_registry_request() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(CACHE_FILE);
        write_cache(&path, &cache("2.0.0", Some("2.0.0"))).unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&requests);
        let current = Version::parse("1.0.0").unwrap();
        let candidate = check(&path, &current, 101, move || {
            observed.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(latest("3.0.0", Some("3.0.0")))
        });
        assert_eq!(candidate.unwrap(), Version::parse("2.0.0").unwrap());
        assert_eq!(requests.load(AtomicOrdering::SeqCst), 0);
    }

    #[test]
    fn failed_request_is_silent_and_negative_cached() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(CACHE_FILE);
        let current = Version::parse("1.0.0").unwrap();
        assert!(check(&path, &current, 100, || Err(io::Error::other("offline"))).is_none());
        let second = check(&path, &current, 101, || {
            panic!("negative cache should suppress a second request")
        });
        assert!(second.is_none());
        let written = read_cache(&path).unwrap();
        assert_eq!(written.last_attempt, Some(100));
        assert_eq!(written.last_success, None);
    }

    #[test]
    fn corrupt_cache_is_replaced_atomically() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(CACHE_FILE);
        fs::write(&path, b"not json").unwrap();
        let current = Version::parse("1.0.0-alpha.1").unwrap();
        let candidate = check(&path, &current, 200, || Ok(latest("1.0.0-alpha.2", None)));
        assert_eq!(candidate.unwrap(), Version::parse("1.0.0-alpha.2").unwrap());
        assert_eq!(
            read_cache(&path).unwrap().latest_any.as_deref(),
            Some("1.0.0-alpha.2")
        );
    }

    #[test]
    fn oversized_cache_is_ignored_and_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(CACHE_FILE);
        fs::write(&path, vec![b'x'; CACHE_LIMIT as usize + 1]).unwrap();
        let current = Version::parse("1.0.0-alpha.1").unwrap();
        let candidate = check(&path, &current, 200, || Ok(latest("1.0.0-alpha.2", None)));
        assert_eq!(candidate.unwrap(), Version::parse("1.0.0-alpha.2").unwrap());
        assert!(fs::metadata(&path).unwrap().len() < CACHE_LIMIT);
    }

    #[test]
    fn lock_contention_never_blocks_the_command() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(CACHE_FILE);
        let lock_path = temp.path().join(LOCK_FILE);
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)
            .unwrap();
        lock.try_lock_exclusive().unwrap();
        let current = Version::parse("1.0.0").unwrap();
        assert!(
            check(&path, &current, 100, || {
                panic!("a contended checker must not access the network")
            })
            .is_none()
        );
    }

    #[test]
    fn registry_response_ignores_yanked_and_invalid_versions() {
        let body = r#"{"versions":[{"num":"9.0.0","yanked":true},{"num":"broken","yanked":false},{"num":"2.0.0-beta.1","yanked":false},{"num":"1.5.0","yanked":false}]}"#;
        let (url, server) = serve_once(body, Duration::ZERO);
        let result = fetch_registry(&url, Duration::from_secs(1)).unwrap();
        server.join().unwrap();
        assert_eq!(result.any, Version::parse("2.0.0-beta.1").unwrap());
        assert_eq!(result.stable, Some(Version::parse("1.5.0").unwrap()));
    }

    #[test]
    fn registry_timeout_is_a_nonfatal_error() {
        let body = r#"{"versions":[]}"#;
        let (url, server) = serve_once(body, Duration::from_millis(200));
        assert!(fetch_registry(&url, Duration::from_millis(30)).is_err());
        server.join().unwrap();
    }

    fn serve_once(body: &str, delay: Duration) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let body = body.to_owned();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request);
            thread::sleep(delay);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        });
        (
            format!("http://{address}/api/v1/crates/workspace-mgr"),
            server,
        )
    }
}
