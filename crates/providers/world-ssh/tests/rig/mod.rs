//! A real, unprivileged sshd on a random loopback port: generated host and client keys, the
//! client key in `authorized_keys`, a minimal `sshd_config`, all in a temp dir. `Rig::start`
//! returns `None` (after printing why) when this host cannot run one, so tests skip instead of fail.
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use world_ssh::{SshConfig, Target};

pub const SSHD: &str = "/usr/sbin/sshd";

pub struct Rig {
    pub dir: tempfile::TempDir,
    pub port: u16,
    sshd: Child,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = self.sshd.kill();
        let _ = self.sshd.wait();
    }
}

fn keygen(path: &Path) -> Result<(), String> {
    let out = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(path)
        .output()
        .map_err(|e| format!("ssh-keygen not runnable: {e}"))?;
    if out.status.success() { Ok(()) } else { Err(format!("ssh-keygen failed: {}", String::from_utf8_lossy(&out.stderr))) }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

impl Rig {
    pub fn start() -> Option<Rig> {
        match Rig::try_start() {
            Ok(r) => Some(r),
            Err(why) => {
                eprintln!("skipping: cannot run a local sshd ({why})");
                None
            }
        }
    }

    fn try_start() -> Result<Rig, String> {
        if !Path::new(SSHD).exists() {
            return Err(format!("{SSHD} does not exist"));
        }
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let d = dir.path().to_path_buf();
        keygen(&d.join("hostkey"))?;
        keygen(&d.join("client"))?;
        std::fs::copy(d.join("client.pub"), d.join("authorized_keys")).map_err(|e| e.to_string())?;
        std::fs::write(d.join("ssh_config"), "").map_err(|e| e.to_string())?;
        let mut last = String::new();
        for _ in 0..3 {
            let port = free_port();
            for penalties in [true, false] {
                let cfg = d.join("sshd_config");
                let mut text = format!(
                    "Port {port}\nListenAddress 127.0.0.1\nHostKey {h}\nPidFile {p}\nAuthorizedKeysFile {a}\n\
                     PasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nStrictModes no\n\
                     MaxStartups 100\nMaxSessions 100\nLogLevel ERROR\n",
                    h = d.join("hostkey").display(),
                    p = d.join("sshd.pid").display(),
                    a = d.join("authorized_keys").display(),
                );
                if penalties {
                    text.push_str("PerSourcePenalties no\n");
                }
                std::fs::write(&cfg, text).map_err(|e| e.to_string())?;
                let check = Command::new(SSHD).arg("-t").arg("-f").arg(&cfg).output().map_err(|e| e.to_string())?;
                if !check.status.success() {
                    last = String::from_utf8_lossy(&check.stderr).trim().to_string();
                    continue;
                }
                let mut sshd = Command::new(SSHD)
                    .args(["-D", "-e", "-f"])
                    .arg(&cfg)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .map_err(|e| e.to_string())?;
                let stderr = sshd.stderr.take().unwrap();
                let (tx, rx) = mpsc::channel();
                std::thread::spawn(move || {
                    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                        let _ = tx.send(line);
                    }
                });
                // LogLevel ERROR prints nothing on success; wait until the port accepts instead.
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                loop {
                    if let Ok(Some(st)) = sshd.try_wait() {
                        last = format!("sshd exited {st}: {}", rx.try_iter().collect::<Vec<_>>().join(" | "));
                        break;
                    }
                    if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                        let kh = std::fs::read_to_string(d.join("hostkey.pub")).unwrap();
                        std::fs::write(d.join("known_hosts"), format!("[127.0.0.1]:{port} {kh}")).unwrap();
                        return Ok(Rig { dir, port, sshd });
                    }
                    if std::time::Instant::now() > deadline {
                        let _ = sshd.kill();
                        last = "sshd did not start listening within 10s".into();
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                break;
            }
        }
        Err(last)
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// A config for a world whose remote workspace is `<rig>/<ws>` (created by the world).
    pub fn config(&self, ws: &str) -> SshConfig {
        SshConfig {
            target: Target {
                host: "127.0.0.1".into(),
                user: None,
                port: Some(self.port),
                identity: Some(self.path("client")),
                config: Some(self.path("ssh_config")),
                known_hosts: Some(self.path("known_hosts")),
            },
            workspace: self.path(ws).to_string_lossy().into_owned(),
            shadow_dir: self.path("shadow").to_string_lossy().into_owned(),
            control_dir: self.path("c"),
        }
    }
}
