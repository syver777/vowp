//! Error type for starting and serving the daemon.

use vowp_store::StoreError;

/// A failure while selecting a project, binding the listener or serving requests.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    /// The caller's arguments or the selected project are unusable.
    #[error("{0}")]
    Config(String),
    /// The home database could not be read.
    ///
    /// The display text is deliberately generic: a store error can contain a home path.
    #[error("could not read the VOWP home")]
    Store(#[source] StoreError),
    /// A filesystem or network operation failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// The HTTP listener failed.
    #[error("server: {0}")]
    Serve(std::io::Error),
}

impl DaemonError {
    /// Maps a store open/read failure onto a safe, path-free explanation.
    pub(crate) fn from_store(err: StoreError) -> Self {
        match err {
            StoreError::NotFound { what: "database", .. } => {
                Self::Config("VOWP home is not initialised; run `vowp init`".to_owned())
            }
            StoreError::NotFound {
                what: "project database",
                ..
            } => Self::Config("the project database is missing; run `vowp init`".to_owned()),
            StoreError::NotFound { .. } => {
                Self::Config("the project is not registered; run `vowp project ls`".to_owned())
            }
            StoreError::MigrationRequired => Self::Config("the VOWP home needs `vowp migrate`".to_owned()),
            StoreError::SchemaTooNew { found, supported } => Self::Config(format!(
                "the VOWP database schema {found} is newer than this binary supports ({supported})"
            )),
            StoreError::NoHome => Self::Config("set VOWP_HOME or HOME, or pass --home".to_owned()),
            other => Self::Store(other),
        }
    }
}
