use aho_vm::VmError;
use fs2::FileExt;
use serde::{Serialize, de::DeserializeOwned};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub fn error(e: impl std::fmt::Display) -> VmError {
    VmError::Io(e.to_string())
}
pub fn now() -> u64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => u64::try_from(d.as_millis()).unwrap_or(u64::MAX),
        Err(_) => 0,
    }
}
pub fn name(value: &str) -> Result<(), VmError> {
    if value.is_empty() || value.len() > 64 || !value.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_') {
        return Err(VmError::Invalid("names must contain 1–64 ASCII letters, digits, '-' or '_'".into()));
    }
    Ok(())
}

#[allow(clippy::disallowed_methods, reason = "authority: vm-state-directory")]
pub fn directory(path: &Path) -> Result<PathBuf, VmError> {
    std::fs::create_dir_all(path).map_err(error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(error)?;
    }
    std::fs::canonicalize(path).map_err(error)
}
/// Whether a piece of VM state (a machine record, a socket, a lease) is present.
#[allow(clippy::disallowed_methods, reason = "authority: vm-state-exists")]
pub fn exists(path: &Path) -> bool {
    path.exists()
}
#[allow(clippy::disallowed_methods, reason = "authority: vm-state-read")]
pub fn load<T: DeserializeOwned>(path: &Path) -> Result<T, VmError> {
    let file = File::open(path).map_err(error)?;
    if file.metadata().map_err(error)?.len() > 8 << 20 {
        return Err(VmError::Invalid(format!("state {} exceeds 8 MiB", path.display())));
    }
    serde_json::from_reader(file).map_err(error)
}
#[allow(clippy::disallowed_methods, reason = "authority: vm-state-save")]
pub fn save<T: Serialize>(path: &Path, value: &T) -> Result<(), VmError> {
    let parent = path.parent().ok_or_else(|| VmError::Invalid("state path needs a parent".into()))?;
    let temp = parent.join(format!(".write-{}", uuid::Uuid::new_v4()));
    let mut file = OpenOptions::new().write(true).create_new(true).open(&temp).map_err(error)?;
    serde_json::to_writer(&mut file, value).map_err(error)?;
    file.write_all(b"\n").map_err(error)?;
    file.sync_all().map_err(error)?;
    std::fs::rename(&temp, path).map_err(error)?;
    File::open(parent).and_then(|f| f.sync_all()).map_err(error)
}
#[allow(clippy::disallowed_methods, reason = "authority: vm-state-lock")]
pub fn lock(path: &Path) -> Result<File, VmError> {
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path).map_err(error)?;
    file.lock_exclusive().map_err(error)?;
    Ok(file)
}
#[allow(clippy::disallowed_methods, reason = "authority: vm-state-list")]
pub fn entries(path: &Path) -> Result<Vec<PathBuf>, VmError> {
    let mut entries =
        std::fs::read_dir(path).map_err(error)?.map(|entry| entry.map(|e| e.path()).map_err(error)).collect::<Result<Vec<_>, _>>()?;
    entries.sort();
    Ok(entries)
}
#[allow(clippy::disallowed_methods, reason = "authority: vm-media-hash")]
pub fn hash(path: &Path) -> Result<String, VmError> {
    let mut file = File::open(path).map_err(error)?;
    let mut hash = blake3::Hasher::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer).map_err(error)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(hash.finalize().to_hex().to_string())
}
