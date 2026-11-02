//! Errors of mod management. Messages are written for the person using the
//! panel or CLI: no paths, no SQL, no raw network text. They may contain mod
//! titles taken from Modrinth, which every screen escapes.

use super::http::HttpError;

#[derive(Debug, thiserror::Error)]
pub enum ModError {
    #[error("mod management is not set up on this daemon")]
    NotConfigured,
    #[error("{0}")]
    InvalidInput(String),
    #[error("could not talk to Modrinth: {0}")]
    Http(#[from] HttpError),
    #[error("Modrinth sent something unexpected: {0}")]
    BadResponse(String),
    #[error("refusing to download from {0}: only https://cdn.modrinth.com is allowed")]
    ForbiddenHost(String),
    #[error("the download of {file} does not match the checksum Modrinth published")]
    HashMismatch { file: String },
    #[error("{file} is {got} bytes but Modrinth announced {expected}")]
    SizeMismatch {
        file: String,
        expected: u64,
        got: u64,
    },
    #[error("{file} is larger than the allowed {limit_bytes} bytes")]
    TooLarge { file: String, limit_bytes: u64 },
    #[error("{file} is not a usable Fabric mod: {why}")]
    BadFile { file: String, why: String },
    #[error("no version of {project} works with Fabric on Minecraft {mc_version}{hint}")]
    NoCompatibleVersion {
        project: String,
        mc_version: String,
        hint: &'static str,
    },
    #[error("{0} is a client-only mod and cannot run on a server")]
    ClientOnly(String),
    #[error("{0} is not a mod")]
    NotAMod(String),
    #[error("{0}")]
    Conflicts(String),
    #[error("this needs more than {0} mods at once; install them in smaller steps")]
    TooMany(usize),
    #[error("{0} is already installed in this version")]
    AlreadyInstalled(String),
    #[error("{0} is not installed")]
    NotInstalled(String),
    #[error("{title} is needed by {dependents}")]
    HasDependents { title: String, dependents: String },
    #[error("project {0} was not found on Modrinth")]
    ProjectNotFound(String),
    #[error("the server did not come back up with the change, so it was rolled back: {reason}")]
    RolledBack { reason: String },
    #[error("the change failed and rolling back did not work either: {reason}")]
    RollbackFailed { reason: String },
}

impl ModError {
    pub fn invalid(message: impl Into<String>) -> ModError {
        ModError::InvalidInput(message.into())
    }
}
