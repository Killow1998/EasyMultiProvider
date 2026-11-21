use crate::app::{ServerState, TransportState};
use crate::http::response::response;
use emp_state::filesystem::atomic_write_private_state;
use emp_transport::{HttpClient, HttpMethod};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

mod assets;
use assets::{PACK, Resource};

const SOURCE: &str = "https://raw.githubusercontent.com/Killow1998/EasyMultiProvider";
const MANIFEST_LIMIT: usize = 32 * 1024;
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const RETRY_INTERVAL: Duration = Duration::from_secs(10 * 60);

struct Manifest {
    version: u64,
    files: BTreeMap<String, Asset>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestDocument {
    schema_version: u8,
    version: u64,
    files: BTreeMap<String, Entry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    sha256: String,
    size: usize,
}

struct Asset {
    resource: Resource,
    entry: Entry,
}

impl Manifest {
    fn read(bytes: &[u8]) -> Option<Self> {
        let ManifestDocument {
            schema_version,
            version,
            files,
        } = serde_json::from_slice(bytes).ok()?;
        if schema_version != 1 || version == 0 || !files.contains_key(PACK) {
            return None;
        }
        let files = files
            .into_iter()
            .map(|(path, entry)| {
                let resource = Resource::find(&path)?;
                if !(1..=resource.limit()).contains(&entry.size) || !valid_digest(&entry.sha256) {
                    return None;
                }
                Some((path, Asset { resource, entry }))
            })
            .collect::<Option<_>>()?;
        Some(Self { version, files })
    }
}

impl Entry {
    fn matches(&self, bytes: &[u8]) -> bool {
        bytes.len() == self.size && format!("{:x}", Sha256::digest(bytes)) == self.sha256
    }

    fn read(&self, path: &Path) -> Option<Vec<u8>> {
        read_limited(path, self.size).filter(|bytes| self.matches(bytes))
    }
}

pub(crate) struct Guides {
    root: PathBuf,
    source: String,
    interval: Duration,
    retry: Duration,
    status: Mutex<Status>,
}

struct Status {
    current: Option<Manifest>,
    next_check: Option<Instant>,
}

impl Guides {
    pub(crate) fn open(root: PathBuf) -> Self {
        let current = read_limited(&root.join("manifest.json"), MANIFEST_LIMIT)
            .and_then(|b| Manifest::read(&b));
        #[cfg(debug_assertions)]
        let fixture = std::env::var("EMP_GUIDE_TEST_SOURCE")
            .ok()
            .and_then(|value| url::Url::parse(&value).ok())
            .filter(|url| {
                url.scheme() == "http"
                    && matches!(url.host(), Some(url::Host::Ipv4(ip)) if ip.is_loopback())
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.path() == "/"
                    && url.query().is_none()
                    && url.fragment().is_none()
            });
        #[cfg(not(debug_assertions))]
        let fixture: Option<url::Url> = None;
        let (source, interval, retry) = match fixture {
            Some(url) => (
                url.as_str().trim_end_matches('/').to_string(),
                Duration::from_secs(1),
                Duration::from_secs(1),
            ),
            None => (SOURCE.to_string(), CHECK_INTERVAL, RETRY_INTERVAL),
        };
        Self {
            root,
            source,
            interval,
            retry,
            status: Mutex::new(Status {
                current,
                next_check: Some(Instant::now()),
            }),
        }
    }

    fn check(self: &Arc<Self>, transport: &TransportState) {
        let mut status = self.status.lock().expect("guide state");
        if status.next_check.is_none_or(|next| Instant::now() < next) {
            return;
        }
        status.next_check = None;
        drop(status);
        let guides = Arc::clone(self);
        let client = Arc::clone(&transport.client);
        transport.runtime.spawn(async move {
            let installed = matches!(
                tokio::time::timeout(Duration::from_secs(90), guides.refresh(&client)).await,
                Ok(Some(()))
            );
            guides.prune();
            guides.status.lock().expect("guide state").next_check = Some(
                Instant::now()
                    + if installed {
                        guides.interval
                    } else {
                        guides.retry
                    },
            );
        });
    }

    async fn refresh(&self, client: &HttpClient) -> Option<()> {
        let bytes = download(
            client,
            &format!("{}/guide/manifest.json", self.source),
            MANIFEST_LIMIT,
        )
        .await?;
        let manifest = Manifest::read(&bytes)?;
        if self
            .status
            .lock()
            .expect("guide state")
            .current
            .as_ref()
            .is_some_and(|current| current.version >= manifest.version)
        {
            return Some(());
        }
        for (path, Asset { resource, entry }) in &manifest.files {
            let object = self.root.join("objects").join(&entry.sha256);
            if entry.read(&object).is_some() {
                continue;
            }
            if let Some(embedded) = resource.embedded().filter(|bytes| entry.matches(bytes)) {
                atomic_write_private_state(&object, embedded).ok()?;
                continue;
            }
            let url = format!("{}/guide-v{}/content/{path}", self.source, manifest.version);
            let body = download(client, &url, entry.size).await?;
            if !entry.matches(&body) || !resource.valid(&body) {
                return None;
            }
            atomic_write_private_state(&object, &body).ok()?;
        }
        let mut status = self.status.lock().expect("guide state");
        atomic_write_private_state(&self.root.join("manifest.json"), &bytes).ok()?;
        status.current = Some(manifest);
        Some(())
    }

    fn prune(&self) {
        let objects = self.root.join("objects");
        let Ok(files) = std::fs::read_dir(objects) else {
            return;
        };
        let status = self.status.lock().expect("guide state");
        for file in files.flatten() {
            let filename = file.file_name();
            if filename.to_str().is_some_and(valid_digest)
                && !status.current.as_ref().is_some_and(|current| {
                    current
                        .files
                        .values()
                        .any(|asset| asset.entry.sha256.as_str() == filename)
                })
            {
                let _ = std::fs::remove_file(file.path());
            }
        }
    }

    fn cached_asset(&self, path: &str) -> Option<(u64, Vec<u8>)> {
        let status = self.status.lock().expect("guide state");
        let manifest = status.current.as_ref()?;
        let entry = &manifest.files.get(path)?.entry;
        let bytes = entry.read(&self.root.join("objects").join(&entry.sha256))?;
        Some((manifest.version, bytes))
    }
}

#[derive(Serialize)]
struct PublishedPack {
    version: u64,
    #[serde(flatten)]
    content: assets::TutorialPack,
}

pub(crate) fn tutorials(state: &ServerState) -> Vec<u8> {
    let guides = &state.backend.guides;
    guides.check(&state.backend.transport);
    let pack = guides.cached_asset(PACK).and_then(|(version, bytes)| {
        Some(PublishedPack {
            version,
            content: assets::parse_pack(&bytes)?,
        })
    });
    response(
        "HTTP/1.1 200 OK",
        "application/json",
        &serde_json::to_vec(&pack).expect("serializable tutorial pack"),
        &[],
    )
}

pub(crate) fn asset_response(path: &str, state: &ServerState) -> Option<Vec<u8>> {
    let path = if let Some(gif) = path.strip_prefix("/assets/tutorial-") {
        format!("docs/assets/tutorials/{gif}")
    } else {
        path.strip_prefix("/guide/")?.to_owned()
    };
    let resource = Resource::find(&path)?;
    if matches!(resource, Resource::Markdown(_)) {
        state.backend.guides.check(&state.backend.transport);
    }
    let cached = state
        .backend
        .guides
        .cached_asset(&path)
        .map(|(_, bytes)| bytes);
    let bytes = cached.as_deref().or_else(|| resource.embedded())?;
    Some(match resource {
        Resource::Markdown(_) => assets::render_doc(&path, bytes),
        Resource::Gif(_) => response(
            "HTTP/1.1 200 OK",
            "image/gif",
            bytes,
            &[("Cache-Control", "no-store")],
        ),
        Resource::Pack => return None,
    })
}

fn valid_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn read_limited(path: &Path, limit: usize) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= limit).then_some(bytes)
}

async fn download(client: &HttpClient, url: &str, limit: usize) -> Option<Vec<u8>> {
    let incoming = client
        .open(HttpMethod::Get, url, BTreeMap::new(), None, false)
        .await
        .ok()?;
    if incoming.status() != 200 {
        return None;
    }
    incoming.read_limited(limit).await.ok()
}
