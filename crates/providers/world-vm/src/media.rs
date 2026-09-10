//! Operator-only host media and recipe catalog. Agent tools accept catalog names, never paths.
use crate::storage;
use aho_vm::{Acceleration, Architecture, Media, MediaKind, Recipe, VmError};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The operator's library: immutable media (`<dir>/media`) and recipes (`<dir>/recipes`). It
/// needs no session, lease or network, so `aho media` and `aho add recipe` work anywhere.
#[derive(Clone, Debug)]
pub struct Catalog {
    dir: PathBuf,
    qemu_img: PathBuf,
}

impl Catalog {
    pub fn open(dir: &Path, qemu_img: &Path) -> Result<Self, VmError> {
        let dir = storage::directory(dir)?;
        storage::directory(&dir.join("media"))?;
        storage::directory(&dir.join("recipes"))?;
        Ok(Catalog { dir, qemu_img: qemu_img.to_path_buf() })
    }
    pub fn dir(&self) -> &Path {
        &self.dir
    }
    #[allow(clippy::disallowed_methods, reason = "authority: vm-media-import")]
    pub fn import_media(&self, name: &str, source: &Path, kind: MediaKind) -> Result<Media, VmError> {
        storage::name(name)?;
        let _lock = storage::lock(&self.dir.join(".lock"))?;
        let record = self.dir.join("media").join(format!("{name}.json"));
        if record.exists() {
            return Err(VmError::Invalid(format!("media {name} already exists; use a new immutable name")));
        }
        let meta = std::fs::metadata(source).map_err(storage::error)?;
        if !meta.is_file() || meta.len() == 0 {
            return Err(VmError::Invalid("media must be a non-empty regular file".into()));
        }
        if kind == MediaKind::Qcow2 {
            let out =
                Command::new(&self.qemu_img).args(["info", "--output=json", "-f", "qcow2"]).arg(source).output().map_err(storage::error)?;
            if !out.status.success() {
                return Err(VmError::Invalid(format!("qemu-img refused media: {}", String::from_utf8_lossy(&out.stderr))));
            }
            let info: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(storage::error)?;
            if info.get("backing-filename").is_some() || info.get("data-file").is_some() {
                return Err(VmError::Invalid("qcow2 media must be self-contained (no backing image or external data file); flatten it with qemu-img convert before import".into()));
            }
        }
        let path = self.dir.join("media").join(format!("{name}.image"));
        let mut input = std::fs::File::open(source).map_err(storage::error)?;
        let mut output = std::fs::OpenOptions::new().write(true).create_new(true).open(&path).map_err(storage::error)?;
        std::io::copy(&mut input, &mut output).map_err(storage::error)?;
        output.sync_all().map_err(storage::error)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).map_err(storage::error)?;
        }
        let media =
            Media { name: name.into(), kind, hash: storage::hash(&path)?, bytes: output.metadata().map_err(storage::error)?.len(), path };
        storage::save(&record, &media)?;
        Ok(media)
    }
    pub fn media(&self) -> Result<Vec<Media>, VmError> {
        storage::entries(&self.dir.join("media"))?
            .iter()
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .map(|p| storage::load(p))
            .collect()
    }
    pub(crate) fn resolve_media(&self, name: &str, kinds: &[MediaKind]) -> Result<Media, VmError> {
        storage::name(name)?;
        let media: Media = storage::load(&self.dir.join("media").join(format!("{name}.json")))?;
        let expected = self.dir.join("media").join(format!("{name}.image"));
        if media.path != expected || !kinds.contains(&media.kind) || storage::hash(&media.path)? != media.hash {
            return Err(VmError::Locked(format!("media {name} has the wrong type, path, or content hash; reimport it under a new name")));
        }
        Ok(media)
    }
    pub fn put_recipe(&self, recipe: &Recipe) -> Result<(), VmError> {
        validate_recipe(recipe)?;
        self.validate_media(recipe)?;
        let _lock = storage::lock(&self.dir.join(".lock"))?;
        storage::save(&self.dir.join("recipes").join(format!("{}.json", recipe.name)), recipe)
    }
    pub fn recipes(&self) -> Result<Vec<Recipe>, VmError> {
        storage::entries(&self.dir.join("recipes"))?
            .iter()
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .map(|p| storage::load(p))
            .collect()
    }
    pub(crate) fn recipe(&self, name: &str) -> Result<Recipe, VmError> {
        storage::name(name)?;
        let recipe = storage::load(&self.dir.join("recipes").join(format!("{name}.json")))?;
        validate_recipe(&recipe)?;
        self.validate_media(&recipe)?;
        Ok(recipe)
    }
    fn validate_media(&self, recipe: &Recipe) -> Result<(), VmError> {
        for (name, kinds) in [
            (&recipe.base_media, &[MediaKind::Raw, MediaKind::Qcow2][..]),
            (&recipe.cdrom_media, &[MediaKind::Iso][..]),
            (&recipe.kernel_media, &[MediaKind::Kernel][..]),
            (&recipe.initrd_media, &[MediaKind::Initrd][..]),
            (&recipe.firmware_media, &[MediaKind::Firmware][..]),
        ] {
            if let Some(name) = name {
                self.resolve_media(name, kinds)?;
            }
        }
        Ok(())
    }
}

pub fn validate_recipe(recipe: &Recipe) -> Result<(), VmError> {
    storage::name(&recipe.name)?;
    if !(64..=262144).contains(&recipe.memory_mib) || !(1..=128).contains(&recipe.cpus) || recipe.disk_bytes < 1 << 20 {
        return Err(VmError::Invalid("recipe needs 64..262144 MiB, 1..128 vCPUs and a disk of at least 1 MiB".into()));
    }
    if !recipe.workspace.starts_with('/') || recipe.workspace == "/" || recipe.workspace.split('/').any(|p| p == "..") {
        return Err(VmError::Invalid("recipe workspace must be an absolute guest directory other than /, without '..'".into()));
    }
    if recipe.initrd_media.is_some() && recipe.kernel_media.is_none() {
        return Err(VmError::Invalid("initrd requires a kernel".into()));
    }
    if recipe.architecture == Architecture::Aarch64 && recipe.kernel_media.is_none() && recipe.firmware_media.is_none() {
        return Err(VmError::Invalid("aarch64 requires an operator firmware image or a direct-boot kernel".into()));
    }
    if recipe.acceleration == Acceleration::Kvm && !cfg!(target_os = "linux") {
        return Err(VmError::Locked("KVM recipes require Linux".into()));
    }
    if recipe.acceleration == Acceleration::Hvf && !cfg!(target_os = "macos") {
        return Err(VmError::Locked("HVF recipes require macOS".into()));
    }
    if recipe.acceleration != Acceleration::Tcg && ((recipe.architecture == Architecture::Aarch64) != cfg!(target_arch = "aarch64")) {
        return Err(VmError::Locked(
            "hardware acceleration requires matching host and guest architecture; select tcg for cross-architecture emulation".into(),
        ));
    }
    Ok(())
}
