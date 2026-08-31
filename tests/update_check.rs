use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

const TEST_CACHE_ENV: &str = "WORKSPACE_MGR_UPDATE_TEST_CACHE";
const TEST_URL_ENV: &str = "WORKSPACE_MGR_UPDATE_TEST_URL";
const DISABLE_ENV: &str = "WORKSPACE_MGR_UPDATE_CHECK_DISABLE";

#[test]
fn update_notice_is_cached_and_never_contaminates_stdout() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("update-check.json");
    let latest = "999.0.0-alpha.1";
    let body = format!(r#"{{"versions":[{{"num":"{latest}","yanked":false}}]}}"#);
    let (url, server) = serve_once(200, body);

    let first = invoke(&url, &cache, ["--version"]);
    assert!(first.status.success());
    assert_eq!(
        String::from_utf8(first.stdout.clone()).unwrap(),
        format!("workspace-mgr {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(notice(&first), expected_notice(latest));
    server.join().unwrap();

    // The listener is now gone. A second invocation must use the fresh cache,
    // print the notice again, and leave structured stdout valid.
    let runtime = temp.path().join("runtime");
    let second = invoke(
        &url,
        &cache,
        [
            "--format",
            "json",
            "setup",
            "--dry-run",
            "--runtime-dir",
            runtime.to_str().unwrap(),
        ],
    );
    assert!(
        second.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(json["status"], "dry_run");
    assert_eq!(notice(&second), expected_notice(latest));
}

#[test]
fn registry_failure_is_silent_and_negative_cached() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("update-check.json");
    let (url, server) = serve_once(500, "unavailable".to_owned());

    let first = invoke(&url, &cache, ["--version"]);
    assert!(first.status.success());
    assert!(first.stderr.is_empty());
    server.join().unwrap();

    // A failed attempt is cached, so the unavailable endpoint is not retried.
    let second = invoke(&url, &cache, ["--version"]);
    assert!(second.status.success());
    assert!(second.stderr.is_empty());
}

fn invoke<I, S>(url: &str, cache: &std::path::Path, args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    Command::new(assert_cmd::cargo::cargo_bin!("workspace-mgr"))
        .args(args)
        .env(TEST_URL_ENV, url)
        .env(TEST_CACHE_ENV, cache)
        .env_remove(DISABLE_ENV)
        .output()
        .unwrap()
}

fn notice(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

fn expected_notice(latest: &str) -> String {
    format!(
        "workspace-mgr: update available: {} -> {latest}; agent: notify the user and ask before updating\n",
        env!("CARGO_PKG_VERSION")
    )
}

fn serve_once(status: u16, body: String) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "update request was not received");
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("failed to accept update request: {error}"),
            }
        };
        let mut request = [0_u8; 2048];
        let _ = stream.read(&mut request);
        let reason = if status == 200 { "OK" } else { "Server Error" };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
    });
    (
        format!("http://{address}/api/v1/crates/workspace-mgr"),
        server,
    )
}
