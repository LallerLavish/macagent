use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ModeError {
    #[error("mode not found: {0}")]
    NotFound(String),

    #[error("invalid mode name: {0} ({reason})", reason = .1)]
    InvalidName(String, &'static str),

    #[error("config directory unavailable")]
    NoConfigDir,

    #[error("io error at {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("toml parse error in {path:?}: {source}")]
    TomlParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("toml serialize error: {0}")]
    TomlSerialize(#[from] toml::ser::Error),

    #[error("db error: {0}")]
    Db(#[from] crate::modes::summary::db::DbError),

    #[error("app capture failed: {0}")]
    Capture(String),

    #[error("app restore failed: {0}")]
    Restore(String),

    #[error("user cancelled due to unsaved work")]
    UserCancelled,

    #[error("already in a work mode session — run 'leave work mode' first")]
    AlreadyInMode,
}

impl ModeError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io { path: path.into(), source }
    }
}