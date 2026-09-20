use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutput {
    pub fn success(&self) -> bool {
        self.code == 0
    }
}

#[derive(Debug, Clone)]
pub struct ByteOutput {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

#[derive(Debug, Clone)]
pub struct StreamOutput {
    pub code: i32,
    pub stderr: String,
}

pub fn run<I, S>(program: &str, args: I, cwd: &Path) -> Result<CommandOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    run_with(program, args, cwd, &BTreeMap::new(), None, true)
}

pub fn run_unchecked<I, S>(program: &str, args: I, cwd: &Path) -> Result<CommandOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    run_with(program, args, cwd, &BTreeMap::new(), None, false)
}

pub fn run_with<I, S>(
    program: &str,
    args: I,
    cwd: &Path,
    env: &BTreeMap<String, String>,
    input: Option<&str>,
    check: bool,
) -> Result<CommandOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let output = run_bytes(program, args, cwd, env, input.map(str::as_bytes), false)?;
    let result = CommandOutput {
        code: output.code,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: output.stderr,
    };
    if check && !result.success() {
        let detail = if result.stderr.trim().is_empty() {
            result.stdout.trim()
        } else {
            result.stderr.trim()
        };
        return Err(failure(program, result.code, detail));
    }
    Ok(result)
}

/// Runs a command and returns its standard output without text decoding.
pub fn run_bytes<I, S>(
    program: &str,
    args: I,
    cwd: &Path,
    env: &BTreeMap<String, String>,
    input: Option<&[u8]>,
    check: bool,
) -> Result<ByteOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut stdout = Vec::new();
    let finished = stream(program, args, cwd, env, input, check, |chunk| {
        stdout.extend_from_slice(chunk);
        Ok(())
    })?;
    Ok(ByteOutput {
        code: finished.code,
        stdout,
        stderr: finished.stderr,
    })
}

/// Runs a command, handing standard output to `on_stdout` as it arrives.
///
/// Standard input is written and standard error is drained on separate
/// threads so a child that produces output before consuming all of its input
/// cannot deadlock against this process.
pub fn stream<I, S, F>(
    program: &str,
    args: I,
    cwd: &Path,
    env: &BTreeMap<String, String>,
    input: Option<&[u8]>,
    check: bool,
    mut on_stdout: F,
) -> Result<StreamOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
    F: FnMut(&[u8]) -> Result<()>,
{
    let args: Vec<String> = args
        .into_iter()
        .map(|arg| arg.as_ref().to_owned())
        .collect();
    let mut command = Command::new(program);
    command
        .args(&args)
        .current_dir(cwd)
        .envs(env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn().map_err(|source| match source.kind() {
        std::io::ErrorKind::NotFound => Error::MissingCommand(program.to_owned()),
        _ => Error::Io {
            path: cwd.to_path_buf(),
            source,
        },
    })?;
    let stdin = child.stdin.take();
    let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(Error::message("child output was not available"));
    };
    let (status, streamed, written, diagnostics) = std::thread::scope(|scope| {
        let writer = stdin.map(|mut stdin| {
            let bytes = input.unwrap_or_default();
            // The handle is dropped when the thread ends, closing the pipe.
            scope.spawn(move || stdin.write_all(bytes))
        });
        let reader = scope.spawn(move || {
            let mut buffer = Vec::new();
            stderr.read_to_end(&mut buffer).map(|_| buffer)
        });
        let streamed = pump(&mut stdout, &mut on_stdout, cwd);
        if streamed.is_err() {
            let _ = child.kill();
        }
        drop(stdout);
        let status = child.wait();
        let written = writer.map(|handle| handle.join());
        (status, streamed, written, reader.join())
    });
    let status = status.map_err(|source| Error::Io {
        path: cwd.to_path_buf(),
        source,
    })?;
    streamed?;
    let code = status.code().unwrap_or(1);
    let stderr = match diagnostics {
        Ok(Ok(bytes)) => String::from_utf8_lossy(&bytes).into_owned(),
        Ok(Err(source)) => {
            return Err(Error::Io {
                path: cwd.to_path_buf(),
                source,
            });
        }
        Err(_) => return Err(Error::message("child error reader panicked")),
    };
    match written {
        Some(Err(_)) => return Err(Error::message("child input writer panicked")),
        // A child that fails may stop reading early; its exit status is the
        // actionable error rather than the resulting broken pipe.
        Some(Ok(Err(source))) if code == 0 => {
            return Err(Error::Io {
                path: cwd.to_path_buf(),
                source,
            });
        }
        _ => {}
    }
    if check && code != 0 {
        return Err(failure(program, code, stderr.trim()));
    }
    Ok(StreamOutput { code, stderr })
}

fn pump<F>(stdout: &mut impl Read, on_stdout: &mut F, cwd: &Path) -> Result<()>
where
    F: FnMut(&[u8]) -> Result<()>,
{
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = match stdout.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(source) => {
                return Err(Error::Io {
                    path: cwd.to_path_buf(),
                    source,
                });
            }
        };
        on_stdout(&buffer[..read])?;
    }
}

fn failure(program: &str, code: i32, detail: &str) -> Error {
    Error::Command {
        // Arguments may contain repository paths or remote configuration.
        // The child process already supplies the actionable diagnostic.
        command: program.to_owned(),
        code,
        detail: detail.to_owned(),
    }
}

pub fn command_exists(program: &str) -> bool {
    which::which(program).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn within_deadline<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(work());
        });
        receiver
            .recv_timeout(std::time::Duration::from_secs(120))
            .expect("child process exchange deadlocked")
    }

    #[cfg(unix)]
    #[test]
    fn writes_and_closes_child_stdin_before_waiting() {
        let output = run_with(
            "sh",
            ["-c", "read line; printf '%s' \"$line\""],
            Path::new("."),
            &BTreeMap::new(),
            Some("input line\n"),
            true,
        )
        .unwrap();
        assert_eq!(output.stdout, "input line");
    }

    #[cfg(unix)]
    #[test]
    fn streams_input_larger_than_the_pipe_buffer_while_the_child_answers() {
        // A child that answers as it reads fills its output pipe long before
        // this much input is consumed. Writing the input and reading the reply
        // must therefore overlap, or the two processes wait on each other.
        let input = "workspace-mgr\n".repeat(300_000);
        let expected = input.len();
        let output = within_deadline(move || {
            run_with(
                "cat",
                ["-"],
                Path::new("."),
                &BTreeMap::new(),
                Some(&input),
                true,
            )
            .unwrap()
        });
        assert_eq!(output.stdout.len(), expected);
    }

    #[cfg(unix)]
    #[test]
    fn large_input_echoed_by_the_child_does_not_deadlock() {
        let input = (0..3 * 1024 * 1024)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let expected = input.clone();
        let output = within_deadline(move || {
            run_bytes(
                "cat",
                std::iter::empty::<&str>(),
                Path::new("."),
                &BTreeMap::new(),
                Some(&input),
                true,
            )
            .unwrap()
        });
        assert_eq!(output.stdout.len(), expected.len());
        assert!(output.stdout == expected, "binary output was altered");
    }

    #[test]
    fn large_git_batch_exchange_does_not_deadlock() {
        let repository = tempfile::tempdir().unwrap();
        let root = repository.path().to_path_buf();
        run("git", ["init", "-q"], &root).unwrap();
        std::fs::write(root.join("blob.txt"), "content\n").unwrap();
        let oid = run("git", ["hash-object", "-w", "blob.txt"], &root)
            .unwrap()
            .stdout
            .trim()
            .to_owned();
        let count = 40_000;
        let input = format!("{oid}\n").repeat(count);
        let output = within_deadline(move || {
            run_with(
                "git",
                [
                    "cat-file",
                    "--batch-check=%(objectname) %(objecttype) %(objectsize)",
                ],
                &root,
                &BTreeMap::new(),
                Some(&input),
                true,
            )
            .unwrap()
        });
        assert!(output.stdout.len() > 1024 * 1024);
        assert_eq!(output.stdout.lines().count(), count);
        assert!(
            output
                .stdout
                .lines()
                .all(|line| line == format!("{oid} blob 8"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn child_failure_is_reported_instead_of_a_broken_pipe() {
        let input = vec![b'x'; 2 * 1024 * 1024];
        let unchecked = input.clone();
        let error = within_deadline(move || {
            run_bytes(
                "sh",
                ["-c", "echo refused >&2; exit 3"],
                Path::new("."),
                &BTreeMap::new(),
                Some(&input),
                true,
            )
            .unwrap_err()
        });
        match error {
            Error::Command { code, detail, .. } => {
                assert_eq!(code, 3);
                assert_eq!(detail, "refused");
            }
            other => panic!("unexpected error: {other}"),
        }
        let output = within_deadline(move || {
            run_bytes(
                "sh",
                ["-c", "exit 4"],
                Path::new("."),
                &BTreeMap::new(),
                Some(&unchecked),
                false,
            )
            .unwrap()
        });
        assert_eq!(output.code, 4);
    }

    #[cfg(unix)]
    #[test]
    fn stream_consumer_errors_stop_the_child() {
        let input = vec![b'y'; 4 * 1024 * 1024];
        let error = within_deadline(move || {
            let mut seen = 0_usize;
            stream(
                "cat",
                std::iter::empty::<&str>(),
                Path::new("."),
                &BTreeMap::new(),
                Some(&input),
                true,
                |chunk| {
                    seen += chunk.len();
                    Err(Error::message(format!("stop after {seen} bytes")))
                },
            )
            .unwrap_err()
        });
        assert!(error.to_string().starts_with("stop after"));
    }
}
