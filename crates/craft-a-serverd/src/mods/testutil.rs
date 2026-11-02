//! Test support: a fake Modrinth transport serving fixtures and synthetic
//! projects. No test touches the network.

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Value, json};
use sha2::{Digest, Sha512};

use super::ModError;
use super::client::ModrinthClient;
use super::files::testutil::fabric_jar;
use super::http::{API_BASE, HttpError, HttpMeta, HttpRequest, Transport};
use super::model::Version;
use super::resolver::VersionSource;

/// What the fake answers for a matching URL.
#[derive(Debug, Clone)]
pub enum Reply {
    Json(Vec<u8>),
    Status(u16),
    Redirect(u16),
    /// A body of this many `a` bytes, regardless of what was announced.
    Flood(usize),
    NetworkError,
}

#[derive(Debug, Default)]
struct Inner {
    /// (url prefix, replies served in order; the last one repeats)
    routes: Vec<(String, Vec<Reply>)>,
    requests: Vec<HttpRequest>,
}

#[derive(Debug, Default, Clone)]
pub struct FakeTransport {
    inner: Arc<Mutex<Inner>>,
}

impl FakeTransport {
    pub fn new() -> FakeTransport {
        FakeTransport::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Serves `replies` for URLs starting with `prefix` (the longest matching
    /// prefix wins). Replaces an earlier route with the same prefix.
    pub fn route(&self, prefix: &str, replies: Vec<Reply>) {
        let mut g = self.lock();
        g.routes.retain(|(p, _)| p != prefix);
        g.routes.insert(0, (prefix.to_owned(), replies));
    }

    pub fn api(&self, path_prefix: &str, json: &Value) {
        self.route(
            &format!("{API_BASE}{path_prefix}"),
            vec![Reply::Json(json.to_string().into_bytes())],
        );
    }

    pub fn cdn(&self, url: &str, bytes: &[u8]) {
        self.route(url, vec![Reply::Json(bytes.to_vec())]);
    }

    pub fn requests(&self) -> Vec<HttpRequest> {
        self.lock().requests.clone()
    }

    pub fn request_urls(&self) -> Vec<String> {
        self.requests().into_iter().map(|r| r.url).collect()
    }

    /// A client that never sleeps.
    pub fn client(&self) -> Arc<ModrinthClient> {
        Arc::new(ModrinthClient::new(Arc::new(self.clone())).with_sleeper(|_| {}))
    }
}

impl Transport for FakeTransport {
    fn execute(&self, req: &HttpRequest, sink: &mut dyn Write) -> Result<HttpMeta, HttpError> {
        let reply = {
            let mut g = self.lock();
            g.requests.push(req.clone());
            let route = g
                .routes
                .iter_mut()
                .filter(|(p, _)| req.url.starts_with(p.as_str()))
                .max_by_key(|(p, _)| p.len());
            match route {
                Some((_, replies)) if replies.len() > 1 => Some(replies.remove(0)),
                Some((_, replies)) => replies.first().cloned(),
                None => None,
            }
        };
        let meta = |status| HttpMeta {
            status,
            retry_after_secs: None,
            ratelimit_remaining: None,
        };
        match reply {
            None => Ok(meta(404)),
            Some(Reply::Status(s)) => Ok(meta(s)),
            Some(Reply::Redirect(s)) => Err(HttpError::Redirect(s)),
            Some(Reply::NetworkError) => Err(HttpError::Network("connection reset".into())),
            Some(Reply::Json(bytes)) => {
                if bytes.len() as u64 > req.max_bytes {
                    return Err(HttpError::TooLarge(req.max_bytes));
                }
                sink.write_all(&bytes)
                    .map_err(|e| HttpError::Sink(e.to_string()))?;
                Ok(meta(200))
            }
            Some(Reply::Flood(n)) => {
                // A transport that does not enforce the cap itself.
                sink.write_all(&vec![b'a'; n])
                    .map_err(|e| HttpError::Sink(e.to_string()))?;
                Ok(meta(200))
            }
        }
    }
}

/// A recorded fixture file.
pub fn fixture(name: &str) -> Value {
    let path = format!(
        "{}/tests/fixtures/modrinth/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(&path).unwrap_or_else(|_| "null".to_owned());
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

pub fn versions_fixture(name: &str) -> Vec<Version> {
    serde_json::from_value(fixture(name)).unwrap_or_default()
}

/// One synthetic version with a real, matching jar.
#[derive(Debug, Clone)]
pub struct Synth {
    pub project: String,
    pub version: String,
    pub number: String,
    pub kind: &'static str,
    pub date: String,
    pub file_name: String,
    pub jar: Vec<u8>,
    /// (project, pinned version, type)
    pub deps: Vec<(String, Option<String>, &'static str)>,
    pub mc: String,
}

impl Synth {
    pub fn new(project: &str, version: &str) -> Synth {
        Synth {
            project: project.to_owned(),
            version: version.to_owned(),
            number: "1.0.0".to_owned(),
            kind: "release",
            date: "2026-10-01T00:00:00Z".to_owned(),
            file_name: format!("{project}-1.0.0.jar"),
            jar: fabric_jar(project.as_bytes()),
            deps: Vec::new(),
            mc: "26.3".to_owned(),
        }
    }

    #[must_use]
    pub fn dep(mut self, project: &str, kind: &'static str) -> Synth {
        self.deps.push((project.to_owned(), None, kind));
        self
    }

    #[must_use]
    pub fn number(mut self, n: &str) -> Synth {
        n.clone_into(&mut self.number);
        self
    }

    #[must_use]
    pub fn kind(mut self, k: &'static str) -> Synth {
        self.kind = k;
        self
    }

    pub fn url(&self) -> String {
        format!(
            "https://cdn.modrinth.com/data/{}/versions/{}/{}",
            self.project, self.version, self.file_name
        )
    }

    pub fn sha512(&self) -> String {
        hex::encode(Sha512::digest(&self.jar))
    }

    pub fn json(&self) -> Value {
        json!({
            "id": self.version,
            "project_id": self.project,
            "version_number": self.number,
            "version_type": self.kind,
            "status": "listed",
            "loaders": ["fabric"],
            "game_versions": [self.mc],
            "date_published": self.date,
            "files": [{
                "hashes": {"sha1": "x", "sha512": self.sha512()},
                "url": self.url(),
                "filename": self.file_name,
                "primary": true,
                "size": self.jar.len(),
            }],
            "dependencies": self.deps.iter().map(|(p, v, t)| json!({
                "project_id": p, "version_id": v, "file_name": null, "dependency_type": t
            })).collect::<Vec<_>>(),
        })
    }

    pub fn version(&self) -> Version {
        serde_json::from_value(self.json()).unwrap_or_else(|_| unreachable_version())
    }
}

fn unreachable_version() -> Version {
    // Synth::json always deserializes; this only keeps the signature total.
    Version {
        id: String::new(),
        project_id: String::new(),
        version_number: String::new(),
        version_type: super::model::VersionType::Other,
        status: String::new(),
        loaders: Vec::new(),
        game_versions: Vec::new(),
        date_published: String::new(),
        files: Vec::new(),
        dependencies: Vec::new(),
    }
}

/// Serves a synthetic project on the fake transport: project, versions and jar.
pub fn serve_synth(t: &FakeTransport, title: &str, server_side: &str, versions: &[Synth]) {
    let Some(first) = versions.first() else {
        return;
    };
    let p = &first.project;
    t.api(
        &format!("/project/{p}/version"),
        &Value::Array(versions.iter().map(Synth::json).collect()),
    );
    t.api(
        &format!("/project/{p}"),
        &json!({
            "id": p, "slug": p.to_ascii_lowercase(), "title": title,
            "description": "d", "project_type": "mod",
            "client_side": "optional", "server_side": server_side,
            "loaders": ["fabric"],
        }),
    );
    for v in versions {
        t.api(&format!("/version/{}", v.version), &v.json());
        t.cdn(&v.url(), &v.jar);
    }
}

/// An in-memory [`VersionSource`] over synthetic or fixture versions.
#[derive(Debug, Default)]
pub struct MapSource {
    pub by_project: HashMap<String, Vec<Version>>,
    pub by_id: HashMap<String, Version>,
    pub calls: Mutex<Vec<String>>,
}

impl MapSource {
    pub fn add(&mut self, versions: Vec<Version>) {
        for v in versions {
            self.by_id.insert(v.id.clone(), v.clone());
            self.by_project
                .entry(v.project_id.clone())
                .or_default()
                .push(v);
        }
    }
}

impl VersionSource for MapSource {
    fn project_versions(&self, project_id: &str) -> Result<Vec<Version>, ModError> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(project_id.to_owned());
        Ok(self.by_project.get(project_id).cloned().unwrap_or_default())
    }

    fn version(&self, version_id: &str) -> Result<Option<Version>, ModError> {
        Ok(self.by_id.get(version_id).cloned())
    }
}
