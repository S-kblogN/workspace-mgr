#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

/// Test builds may lower the cloud-usage approval threshold through this
/// variable. Every helper clears it unless a test passes it explicitly.
pub const CLOUD_USAGE_THRESHOLD_ENV: &str = "WORKSPACE_MGR_TEST_CLOUD_USAGE_THRESHOLD_BYTES";

/// Test builds may stand in for another workspace-mgr release through this
/// variable. Every helper clears it unless a test passes it explicitly.
pub const CLI_VERSION_ENV: &str = "WORKSPACE_MGR_TEST_CLI_VERSION";

pub struct GitFixture {
    pub temp: TempDir,
    pub root: PathBuf,
    pub remote: PathBuf,
    pub seed: PathBuf,
    pub shared: PathBuf,
}

impl GitFixture {
    pub fn new() -> Self {
        let temp = tempfile::tempdir().expect("temporary fixture");
        let root = temp.path().to_path_buf();
        let remote = root.join("remote.git");
        let seed = root.join("seed");
        let shared = root.join("shared");
        command(&root, "git", ["init", "--bare", remote.to_str().unwrap()]);
        command(&root, "git", ["init", "-b", "main", seed.to_str().unwrap()]);
        configure_git(&seed);
        std::fs::write(seed.join("README.md"), "base\n").unwrap();
        git(&seed, ["add", "README.md"]);
        git(&seed, ["commit", "-m", "Initial main"]);
        git(&seed, ["remote", "add", "origin", remote.to_str().unwrap()]);
        git(&seed, ["push", "-u", "origin", "main"]);
        git(&remote, ["symbolic-ref", "HEAD", "refs/heads/main"]);
        Self {
            temp,
            root,
            remote,
            seed,
            shared,
        }
    }

    pub fn clone_shared(&self) {
        command(
            &self.root,
            "git",
            [
                "clone",
                self.remote.to_str().unwrap(),
                self.shared.to_str().unwrap(),
            ],
        );
        configure_git(&self.shared);
    }

    pub fn commit_seed(&self, message: &str) {
        git(&self.seed, ["add", "-A"]);
        git(&self.seed, ["commit", "-m", message]);
        git(&self.seed, ["push", "origin", "main"]);
    }
}

pub fn configure_git(repo: &Path) {
    git(repo, ["config", "user.name", "Workspace Mgr Test"]);
    git(
        repo,
        ["config", "user.email", "workspace-mgr@example.invalid"],
    );
}

pub fn git<I, S>(repo: &Path, args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut full = vec!["-C".into(), repo.as_os_str().to_owned()];
    full.extend(args.into_iter().map(|arg| arg.as_ref().to_owned()));
    command_os(repo, "git", full)
}

pub fn git_unchecked<I, S>(repo: &Path, args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut full = vec!["-C".into(), repo.as_os_str().to_owned()];
    full.extend(args.into_iter().map(|arg| arg.as_ref().to_owned()));
    Command::new("git")
        .args(full)
        .current_dir(repo)
        .output()
        .expect("run git")
}

pub fn command<I, S>(cwd: &Path, program: &str, args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    command_os(
        cwd,
        program,
        args.into_iter().map(|arg| arg.as_ref().to_owned()),
    )
}

fn command_os<I, S>(cwd: &Path, program: &str, args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    checked(program, configured(cwd, program, args, &[]))
}

fn configured<I, S>(cwd: &Path, program: &str, args: I, env: &[(&str, &str)]) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .env("WORKSPACE_MGR_FORMAT", "json")
        .env("WORKSPACE_MGR_UPDATE_CHECK_DISABLE", "1")
        .env_remove(CLOUD_USAGE_THRESHOLD_ENV)
        .env_remove(CLI_VERSION_ENV);
    inject_test_storage_engine(&mut command);
    command.envs(env.iter().copied());
    command
}

fn checked(program: &str, mut command: Command) -> Output {
    let output = command.output().expect("run command");
    if !output.status.success() {
        panic!(
            "{program} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    output
}

pub fn binary() -> PathBuf {
    PathBuf::from(assert_cmd::cargo::cargo_bin!("workspace-mgr"))
}

/// A workspace-mgr command that ignores the developer's environment: both
/// test overrides are cleared and the update check is disabled. Tests that
/// spawn the binary directly start from this instead of `Command::new`, then
/// add their own arguments, directory, and environment.
pub fn binary_command() -> Command {
    let mut command = Command::new(binary());
    command
        .env("WORKSPACE_MGR_UPDATE_CHECK_DISABLE", "1")
        .env_remove(CLOUD_USAGE_THRESHOLD_ENV)
        .env_remove(CLI_VERSION_ENV);
    command
}

pub fn workspace<I, S>(cwd: &Path, args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    command_os(cwd, binary().to_str().unwrap(), args)
}

pub fn workspace_unchecked<I, S>(cwd: &Path, args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    workspace_env_unchecked(cwd, args, &[])
}

/// Runs workspace-mgr like [`workspace`] with extra environment variables,
/// which are applied last and may replace the injected storage engine.
pub fn workspace_env<I, S>(cwd: &Path, args: I, env: &[(&str, &str)]) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let program = binary();
    let program = program.to_str().unwrap();
    checked(program, configured(cwd, program, args, env))
}

pub fn workspace_env_unchecked<I, S>(cwd: &Path, args: I, env: &[(&str, &str)]) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    configured(cwd, binary().to_str().unwrap(), args, env)
        .output()
        .expect("run workspace-mgr")
}

fn inject_test_storage_engine(_command: &mut Command) {
    #[cfg(feature = "test-storage")]
    if let Ok(program) = which::which("dvc") {
        _command.env("WORKSPACE_MGR_STORAGE_DVC", program);
    }
}

/// Records something in the task's own documentation, which publication
/// requires before a deliverable task may publish substantive content. The
/// record is a file of its own rather than an addition to the README, because
/// the fixed policy keeps a task README concise and out of the
/// chronological-log role; these fixtures are the product's own worked examples
/// of the shape it asks for.
pub fn document_task(task: &Path) {
    let record = task.join("record.md");
    std::fs::write(
        &record,
        "# Record\n\nThis fixture publishes content, so the task records it here.\n",
    )
    .expect("write task record");
}

pub fn json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid JSON: {error}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}
