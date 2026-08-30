use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Message(String),

    #[error("failed to read or write {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid TOML in {path}: {source}")]
    Toml {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("invalid YAML in {path}: {source}")]
    Yaml {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },

    #[error("required command is not available: {0}")]
    MissingCommand(String),

    #[error("{command} failed with exit code {code}: {detail}")]
    Command {
        command: String,
        code: i32,
        detail: String,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn message(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}

pub trait IoContext<T> {
    fn at(self, path: impl Into<PathBuf>) -> Result<T>;
}

impl<T> IoContext<T> for std::io::Result<T> {
    fn at(self, path: impl Into<PathBuf>) -> Result<T> {
        let path = path.into();
        self.map_err(|source| Error::Io { path, source })
    }
}
