use aho_net::{NetError, NetworkConfig, Nic};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Saved {
    pub version: u32,
    pub config: NetworkConfig,
    pub nics: Vec<Nic>,
}
pub(crate) struct Store {
    dir: PathBuf,
    _lock: File,
    generation: u64,
}
impl Store {
    pub fn open(dir: &Path, initial: NetworkConfig) -> Result<(Self, Saved), NetError> {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
        if !dir.exists() {
            fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        }
        let metadata = fs::symlink_metadata(dir)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() || metadata.permissions().mode() & 0o077 != 0 {
            return Err(NetError::Invalid("network state must be a private (0700), non-symlink directory".into()));
        }
        let lock_path = dir.join("network.lock");
        if fs::symlink_metadata(&lock_path).is_ok_and(|m| !m.is_file() || m.file_type().is_symlink()) {
            return Err(NetError::Invalid("network lock must be a regular file".into()));
        }
        let lock = OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(lock_path)?;
        lock.try_lock_exclusive().map_err(|e| NetError::Unavailable(format!("network is already open: {e}")))?;
        let state_path = dir.join("network.json");
        let saved = match fs::symlink_metadata(&state_path) {
            Ok(meta) => {
                if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 4 * 1024 * 1024 {
                    return Err(NetError::Invalid("invalid network state file".into()));
                }
                let mut text = String::new();
                File::open(&state_path)?.take(4 * 1024 * 1024 + 1).read_to_string(&mut text)?;
                let saved: Saved = serde_json::from_str(&text).map_err(|e| NetError::Invalid(format!("network state: {e}")))?;
                if saved.version != 1 || saved.config.id != initial.id {
                    return Err(NetError::Invalid("network state version/id mismatch".into()));
                }
                saved
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Saved { version: 1, config: initial, nics: vec![] },
            Err(e) => return Err(e.into()),
        };
        let mut store = Self { dir: dir.to_owned(), _lock: lock, generation: 0 };
        store.save(&saved)?;
        Ok((store, saved))
    }
    pub fn save(&mut self, saved: &Saved) -> Result<(), NetError> {
        use std::os::unix::fs::OpenOptionsExt;
        let bytes = serde_json::to_vec_pretty(saved).map_err(|e| NetError::Invalid(e.to_string()))?;
        let (path, mut file) = loop {
            self.generation += 1;
            let path = self.dir.join(format!("network-{}-{}.new", std::process::id(), self.generation));
            match OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path) {
                Ok(file) => break (path, file),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        };
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(path, self.dir.join("network.json"))?;
        File::open(&self.dir)?.sync_all()?;
        Ok(())
    }
}

pub(crate) fn validate(config: &NetworkConfig) -> Result<(), NetError> {
    if config.id.is_empty() || config.id.len() > 128 || config.max_flows == 0 || config.max_flows > 16384 {
        return Err(NetError::Invalid("network id/max_flows outside supported bounds".into()));
    }
    if config.ipv4.prefix_len() != 24
        || !config.ipv4.addr().is_private()
        || config.ipv4.addr() != config.ipv4.network()
        || config.ipv6.prefix_len() != 64
        || config.ipv6.addr().segments()[0] & 0xfe00 != 0xfc00
        || config.ipv6.addr() != config.ipv6.network()
    {
        return Err(NetError::Invalid("network needs a canonical private IPv4 /24 and unique-local IPv6 /64".into()));
    }
    if config.dns_upstream.is_empty()
        || config.dns_upstream.len() > 8
        || config.dns_upstream.iter().any(|s| s.port() == 0 || s.ip().is_unspecified() || s.ip().is_multicast())
    {
        return Err(NetError::Invalid("one to eight concrete DNS resolvers are required".into()));
    }
    for destination in config.egress.allow.iter().chain(&config.egress.deny) {
        if let aho_net::Destination::Domain(name) = destination {
            aho_net::Destination::parse(name)?;
        }
    }
    if config.port_maps.len() > 128 {
        return Err(NetError::Invalid("at most 128 port maps".into()));
    }
    for (index, map) in config.port_maps.iter().enumerate() {
        valid_name(&map.guest)?;
        if map.host.port() == 0
            || map.guest_port == 0
            || map.host.ip().is_multicast()
            || config.port_maps[..index].iter().any(|m| m.protocol == map.protocol && m.host == map.host)
        {
            return Err(NetError::Invalid("invalid or duplicate port map".into()));
        }
    }
    Ok(())
}
pub(crate) fn valid_name(name: &str) -> Result<(), NetError> {
    if name.is_empty()
        || name.len() > 63
        || name.starts_with('-')
        || name.ends_with('-')
        || !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(NetError::Invalid("NIC name must be a lowercase DNS label".into()));
    }
    Ok(())
}
pub(crate) fn allocate(config: &NetworkConfig, name: &str, index: usize) -> Result<Nic, NetError> {
    valid_name(name)?;
    if index >= 244 {
        return Err(NetError::Unavailable("network IPv4 address pool exhausted".into()));
    }
    let host = index as u32 + 10;
    let v4 = u32::from(config.ipv4.network());
    let v6 = u128::from(config.ipv6.network());
    let hash = config.id.bytes().fold(0x811c9dc5u32, |h, b| (h ^ u32::from(b)).wrapping_mul(16777619));
    Ok(Nic {
        name: name.into(),
        mac: [2, 0xa0, (hash >> 16) as u8, (hash >> 8) as u8, hash as u8, host as u8],
        ipv4: (v4 + host).into(),
        ipv6: (v6 + u128::from(host)).into(),
        gateway_v4: (v4 + 1).into(),
        gateway_v6: (v6 + 1).into(),
        prefix_v4: 24,
        prefix_v6: 64,
    })
}

/// Prefix indices (`NetworkConfig::prefix_index`) already persisted by other networks under
/// `sessions_dir` (`<id>.net/network.json`), so a new network never shares a subnet with a
/// session that may resume beside it. Unreadable neighbours are skipped: they cannot resume either.
pub(crate) fn persisted_prefixes(sessions_dir: &Path, except: &Path) -> Vec<u8> {
    let Ok(entries) = fs::read_dir(sessions_dir) else { return Vec::new() };
    entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p != except && p.extension().is_some_and(|x| x == "net"))
        .filter_map(|p| {
            let mut text = String::new();
            File::open(p.join("network.json")).ok()?.take(4 * 1024 * 1024).read_to_string(&mut text).ok()?;
            serde_json::from_str::<Saved>(&text).ok().map(|s| s.config.prefix_index())
        })
        .collect()
}

/// Whether a network was already created in `dir` (its settings then win on open).
pub(crate) fn exists(dir: &Path) -> bool {
    fs::symlink_metadata(dir.join("network.json")).is_ok()
}

/// Removes a stopped network's state directory; refused while any process runs it (the lock).
/// `Ok(false)` when there is nothing there.
pub fn remove(dir: &Path) -> Result<bool, NetError> {
    let metadata = match fs::symlink_metadata(dir) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(NetError::Invalid(format!("{} is not a network state directory", dir.display())));
    }
    let lock_path = dir.join("network.lock");
    if fs::symlink_metadata(&lock_path).is_ok() {
        let lock = OpenOptions::new().read(true).write(true).open(&lock_path)?;
        lock.try_lock_exclusive().map_err(|_| NetError::Unavailable("the network is running; stop its session first".into()))?;
        fs::remove_dir_all(dir)?;
        drop(lock);
    } else {
        fs::remove_dir_all(dir)?;
    }
    Ok(true)
}
