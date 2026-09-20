use std::collections::BTreeMap;
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
    let stdin = if input.is_some() {
        Some(
            child
                .stdin
                .take()
                .ok_or_else(|| Error::message("child stdin was not available"))?,
        )
    } else {
        None
    };
    // Input is written on its own thread while this one drains the child's
    // output. Writing it all first deadlocks as soon as the child's reply
    // outgrows the pipe buffer: the child blocks writing, stops reading, and
    // the remaining input has nowhere to go. Commands that answer per input
    // line, such as `git check-ignore --stdin`, reach that size easily.
    let mut write_error = None;
    let waited = std::thread::scope(|scope| {
        let writer = match (input, stdin) {
            (Some(input), Some(mut stdin)) => Some(scope.spawn(move || {
                use std::io::Write;
                let result = stdin.write_all(input.as_bytes());
                // Dropping the handle closes the pipe, so the child sees the
                // end of its input without a second signal.
                drop(stdin);
                result
            })),
            _ => None,
        };
        let waited = child.wait_with_output();
        if let Some(writer) = writer {
            if let Ok(Err(error)) = writer.join() {
                write_error = Some(error);
            }
        }
        waited
    });
    let output = waited.map_err(|source| Error::Io {
        path: cwd.to_path_buf(),
        source,
    })?;
    if let Some(source) = write_error {
        // The child stopped reading before its input ran out, so the command
        // never saw everything it was given.
        return Err(Error::Io {
            path: cwd.to_path_buf(),
            source,
        });
    }
    let result = CommandOutput {
        code: output.status.code().unwrap_or(1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    };
    if check && !result.success() {
        let detail = if result.stderr.trim().is_empty() {
            result.stdout.trim()
        } else {
            result.stderr.trim()
        };
        return Err(Error::Command {
            // Arguments may contain repository paths or remote configuration.
            // The child process already supplies the actionable diagnostic.
            command: program.to_owned(),
            code: result.code,
            detail: detail.to_owned(),
        });
    }
    Ok(result)
}

pub fn command_exists(program: &str) -> bool {
    which::which(program).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let output = run_with(
            "cat",
            ["-"],
            Path::new("."),
            &BTreeMap::new(),
            Some(&input),
            true,
        )
        .unwrap();
        assert_eq!(output.stdout.len(), input.len());
    }
}
