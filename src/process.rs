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
    if let Some(input) = input {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .expect("stdin was piped")
            .write_all(input.as_bytes())
            .map_err(|source| Error::Io {
                path: cwd.to_path_buf(),
                source,
            })?;
    }
    let output = child.wait_with_output().map_err(|source| Error::Io {
        path: cwd.to_path_buf(),
        source,
    })?;
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
