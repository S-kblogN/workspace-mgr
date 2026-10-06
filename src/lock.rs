use std::fs::{self, File};

use crate::error::{IoContext, Result};
use crate::git::GitRepo;
use crate::local_state;

pub struct RepositoryLock {
    _file: File,
    _legacy_files: Vec<File>,
}

impl RepositoryLock {
    pub fn acquire(repo: &GitRepo) -> Result<Self> {
        let directory = local_state::directory_unmigrated(repo)?;
        fs::create_dir_all(&directory).at(&directory)?;
        let file = local_state::open_lock(&directory.join("repository.lock"), true)?;
        let legacy_files = local_state::migrate(repo, &directory)?;
        Ok(Self {
            _file: file,
            _legacy_files: legacy_files,
        })
    }
}
