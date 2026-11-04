//! Modrinth API v2 response types.
//!
//! Everything here comes from the network and is untrusted: fields are
//! optional where the API may omit them, unknown fields are ignored, and ids
//! must pass [`valid_id`] before they are put into a URL or a path.

use serde::Deserialize;

/// Modrinth ids are short base-62 strings; slugs also allow `-`, `_` and `.`.
/// Anything outside this set never reaches a URL or a file name.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

#[derive(Debug, Clone, Deserialize)]
pub struct SearchPage {
    #[serde(default)]
    pub hits: Vec<SearchHit>,
    #[serde(default)]
    pub offset: u64,
    #[serde(default)]
    pub total_hits: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SearchHit {
    pub project_id: String,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub downloads: u64,
    #[serde(default)]
    pub client_side: String,
    #[serde(default)]
    pub server_side: String,
    #[serde(default)]
    pub project_type: String,
    #[serde(default)]
    pub icon_url: Option<String>,
    #[serde(default)]
    pub categories: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Project {
    pub id: String,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub client_side: String,
    #[serde(default)]
    pub server_side: String,
    #[serde(default)]
    pub project_type: String,
    #[serde(default)]
    pub loaders: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VersionType {
    Release,
    Beta,
    Alpha,
    #[serde(other)]
    Other,
}

impl VersionType {
    pub fn as_str(self) -> &'static str {
        match self {
            VersionType::Release => "release",
            VersionType::Beta => "beta",
            VersionType::Alpha => "alpha",
            VersionType::Other => "other",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DependencyType {
    Required,
    Optional,
    Incompatible,
    Embedded,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Dependency {
    #[serde(default)]
    pub version_id: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
    pub dependency_type: DependencyType,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Hashes {
    #[serde(default)]
    pub sha512: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VersionFile {
    #[serde(default)]
    pub hashes: Hashes,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub primary: bool,
    #[serde(default)]
    pub size: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Version {
    pub id: String,
    pub project_id: String,
    #[serde(default)]
    pub version_number: String,
    pub version_type: VersionType,
    #[serde(default = "listed")]
    pub status: String,
    #[serde(default)]
    pub loaders: Vec<String>,
    #[serde(default)]
    pub game_versions: Vec<String>,
    #[serde(default)]
    pub date_published: String,
    #[serde(default)]
    pub files: Vec<VersionFile>,
    #[serde(default)]
    pub dependencies: Vec<Dependency>,
}

fn listed() -> String {
    "listed".to_owned()
}

impl Version {
    /// Both ids are used in URLs and records, so a hostile response must not
    /// get odd characters through.
    pub fn ids_are_valid(&self) -> bool {
        valid_id(&self.id) && valid_id(&self.project_id)
    }

    pub fn supports(&self, mc_version: &str) -> bool {
        self.loaders.iter().any(|l| l == "fabric")
            && self.game_versions.iter().any(|v| v == mc_version)
    }

    /// The file to install: the primary one, else the first that is not a
    /// sources, javadoc or dev jar.
    pub fn install_file(&self) -> Option<&VersionFile> {
        self.files.iter().find(|f| f.primary).or_else(|| {
            self.files.iter().find(|f| {
                let name = f.filename.to_ascii_lowercase();
                !(name.ends_with("-sources.jar")
                    || name.ends_with("-javadoc.jar")
                    || name.ends_with("-dev.jar"))
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_reject_path_characters() {
        assert!(valid_id("P7dR8mSH"));
        assert!(valid_id("fabric-api"));
        for bad in [
            "",
            "../x",
            "a/b",
            "a b",
            "a?b",
            "a%2f",
            "é",
            &"a".repeat(65),
        ] {
            assert!(!valid_id(bad), "{bad}");
        }
    }

    #[test]
    fn unknown_enum_values_are_tolerated() {
        let d: Dependency = serde_json::from_str(r#"{"dependency_type":"something_new"}"#)
            .unwrap_or(Dependency {
                version_id: None,
                project_id: None,
                dependency_type: DependencyType::Required,
            });
        assert_eq!(d.dependency_type, DependencyType::Unknown);
    }
}
