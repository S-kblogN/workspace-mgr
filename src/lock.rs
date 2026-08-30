use std::fs::{self, File, OpenOptions};

use fs2::FileExt;

use crate::error::{Error, IoContext, Result};
use crate::git::GitRepo;

pub struct RepositoryLock {
    _file: File,
}

impl RepositoryLock {
    pub fn acquire(repo: &GitRepo) -> Result<Self> {
        let common_dir = repo.common_dir()?;
        let path = common_dir.join("workspace-mgr/repository.lock");
        let parent = path
            .parent()
            .ok_or_else(|| Error::message("repository lock has no parent"))?;
        fs::create_dir_all(parent).at(parent)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .at(&path)?;
        file.try_lock_exclusive()
            .map_err(|_| Error::message("another workspace-mgr repository operation is running"))?;
        Ok(Self { _file: file })
    }
}
