use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::browser::random_token;
use crate::engine::now_ms;
use crate::{AuthError, Credential, Engine, ResolvedAuth};

const LOCK_TIMEOUT: Duration = Duration::from_secs(100);
const MAX_STORE_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Default, Serialize, Deserialize)]
struct Document {
    #[serde(default)]
    providers: BTreeMap<String, Credential>,
}

#[derive(Clone, Debug)]
pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub async fn get(&self, provider: &str) -> Result<Option<Credential>, AuthError> {
        let store = self.clone();
        let provider = provider.to_owned();
        blocking(move || {
            let _lock = store.lock()?;
            Ok(store.load()?.providers.remove(&provider))
        })
        .await
    }

    pub async fn provider_ids(&self) -> Result<Vec<String>, AuthError> {
        let store = self.clone();
        blocking(move || {
            let _lock = store.lock()?;
            Ok(store.load()?.providers.into_keys().collect())
        })
        .await
    }

    pub async fn set(&self, provider: &str, credential: Credential) -> Result<(), AuthError> {
        if crate::provider(provider).is_none() {
            return Err(AuthError::UnsupportedProvider);
        }
        validate(&credential)?;
        let store = self.clone();
        let provider = provider.to_owned();
        blocking(move || {
            let _lock = store.lock()?;
            let mut document = store.load()?;
            document.providers.insert(provider, credential);
            store.persist(&document)
        })
        .await
    }

    pub async fn remove(&self, provider: &str) -> Result<bool, AuthError> {
        let store = self.clone();
        let provider = provider.to_owned();
        blocking(move || {
            let _lock = store.lock()?;
            let mut document = store.load()?;
            let removed = document.providers.remove(&provider).is_some();
            if removed {
                store.persist(&document)?;
            }
            Ok(removed)
        })
        .await
    }

    pub async fn resolve(&self, provider: &str, _client: &reqwest::Client) -> Result<ResolvedAuth, AuthError> {
        // Authentication never follows redirects, even if the inference client's policy does.
        self.resolve_with_engine(provider, &Engine::new()?).await
    }

    /// Test seam for local refresh endpoints; normal inference uses `resolve`.
    pub async fn resolve_with_engine(&self, provider: &str, engine: &Engine) -> Result<ResolvedAuth, AuthError> {
        if crate::provider(provider).is_none() {
            return Err(AuthError::UnsupportedProvider);
        }
        let store = self.clone();
        let provider = provider.to_owned();
        let engine = engine.clone();
        // Once a refresh starts, retain the lock and finish persisting rotating tokens even if
        // the requesting model call is cancelled. All network operations remain bounded.
        tokio::spawn(async move {
            let loader = store.clone();
            let (lock, mut document) = blocking(move || {
                let lock = loader.lock()?;
                Ok((lock, loader.load()?))
            })
            .await?;
            let mut credential = document.providers.get(&provider).cloned().ok_or(AuthError::MissingCredential)?;
            validate(&credential)?;
            if credential.expires_at.is_some_and(|expiry| expiry <= now_ms().saturating_add(5 * 60 * 1000)) {
                credential = engine.refresh(&provider, &credential).await?;
                validate(&credential)?;
                document.providers.insert(provider.clone(), credential.clone());
                blocking(move || {
                    let result = store.persist(&document);
                    drop(lock);
                    result
                })
                .await?;
            } else {
                drop(lock);
            }
            crate::providers::resolve(&provider, &credential)
        })
        .await
        .map_err(|_| AuthError::Storage)?
    }

    fn parent(&self) -> Result<&Path, AuthError> {
        if !self.path.is_absolute() {
            return Err(AuthError::InsecureStorage);
        }
        self.path.parent().ok_or(AuthError::InsecureStorage)
    }

    fn lock(&self) -> Result<File, AuthError> {
        let parent = self.parent()?;
        secure_directory(parent)?;
        let name = self.path.file_name().ok_or(AuthError::InsecureStorage)?;
        let mut lock_name = name.to_os_string();
        lock_name.push(".lock");
        let file = private_open(&parent.join(lock_name), true)?;
        let start = Instant::now();
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(file),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if start.elapsed() >= LOCK_TIMEOUT {
                        return Err(AuthError::Timeout);
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => return Err(AuthError::Storage),
            }
        }
    }

    fn load(&self) -> Result<Document, AuthError> {
        let mut file = match private_open(&self.path, false) {
            Ok(file) => file,
            Err(AuthError::MissingCredential) => return Ok(Document::default()),
            Err(error) => return Err(error),
        };
        if file.metadata().map_err(|_| AuthError::Storage)?.len() > MAX_STORE_BYTES {
            return Err(AuthError::Storage);
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(|_| AuthError::Storage)?;
        serde_json::from_slice(&bytes).map_err(|_| AuthError::Storage)
    }

    #[allow(clippy::disallowed_methods, reason = "authority: auth-store-persist")]
    fn persist(&self, document: &Document) -> Result<(), AuthError> {
        let parent = self.parent()?;
        let path = parent.join(format!(".auth-{}.pending", random_token()?));
        // On a failed commit, retain the private pending file for recovery of rotating tokens.
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        private_options(&mut options);
        let mut file = options.open(&path).map_err(|_| AuthError::Storage)?;
        serde_json::to_writer(&mut file, document).map_err(|_| AuthError::Storage)?;
        file.write_all(b"\n").map_err(|_| AuthError::Storage)?;
        file.sync_all().map_err(|_| AuthError::Storage)?;
        fs::rename(&path, &self.path).map_err(|_| AuthError::Storage)?;
        File::open(parent).and_then(|directory| directory.sync_all()).map_err(|_| AuthError::Storage)?;
        Ok(())
    }
}

fn validate(credential: &Credential) -> Result<(), AuthError> {
    if credential.access_token.is_empty() {
        return Err(AuthError::InvalidCredential);
    }
    Ok(())
}

async fn blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T, AuthError> + Send + 'static) -> Result<T, AuthError> {
    tokio::task::spawn_blocking(work).await.map_err(|_| AuthError::Storage)?
}

#[cfg(unix)]
fn private_options(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
}

#[cfg(not(unix))]
fn private_options(_options: &mut OpenOptions) {}

#[allow(clippy::disallowed_methods, reason = "authority: auth-store-open")]
fn private_open(path: &Path, create: bool) -> Result<File, AuthError> {
    let mut options = OpenOptions::new();
    options.read(true).write(create).create(create);
    private_options(&mut options);
    let file = options
        .open(path)
        .map_err(|error| if error.kind() == std::io::ErrorKind::NotFound { AuthError::MissingCredential } else { AuthError::Storage })?;
    check_private(&file.metadata().map_err(|_| AuthError::Storage)?, false)?;
    Ok(file)
}

#[cfg(unix)]
#[allow(clippy::disallowed_methods, reason = "authority: auth-store-directory")]
fn secure_directory(path: &Path) -> Result<(), AuthError> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(path).map_err(|_| AuthError::Storage)?;
    let metadata = fs::symlink_metadata(path).map_err(|_| AuthError::Storage)?;
    if !metadata.is_dir() || metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(AuthError::InsecureStorage);
    }
    // Existing ~/.aho directories predate the auth store and may have mode0755.
    if metadata.mode() & 0o077 != 0 {
        let mut options = OpenOptions::new();
        options.read(true).custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC);
        use std::os::unix::fs::OpenOptionsExt;
        let directory = options.open(path).map_err(|_| AuthError::InsecureStorage)?;
        let actual = directory.metadata().map_err(|_| AuthError::Storage)?;
        if actual.uid() != rustix::process::geteuid().as_raw() {
            return Err(AuthError::InsecureStorage);
        }
        directory.set_permissions(fs::Permissions::from_mode(0o700)).map_err(|_| AuthError::Storage)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn secure_directory(_path: &Path) -> Result<(), AuthError> {
    Err(AuthError::InsecureStorage)
}

#[cfg(unix)]
fn check_private(metadata: &fs::Metadata, directory: bool) -> Result<(), AuthError> {
    use std::os::unix::fs::MetadataExt;
    if metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
        || (if directory { !metadata.is_dir() } else { !metadata.is_file() || metadata.nlink() != 1 })
    {
        return Err(AuthError::InsecureStorage);
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_private(_metadata: &fs::Metadata, _directory: bool) -> Result<(), AuthError> {
    Err(AuthError::InsecureStorage)
}
