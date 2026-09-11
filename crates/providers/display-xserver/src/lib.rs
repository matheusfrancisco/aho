//! Xvfb + x11vnc -inetd in the bound world. Protocol bytes use Subprocess, never
//! ambient host sockets/processes. Install python3, Xvfb and x11vnc in that world.
//! X applications need the DISPLAY/XAUTHORITY returned by `environment()`.
#![forbid(unsafe_code)]

use aho_cap::*;
use aho_display::{
    CaptureRequest, Desktop, DesktopGuardHandle, DesktopHandle, DisplayError, Guard, KeyEvent, PointerEvent, PointerState, Screenshot,
};
use aho_proc::{Child, SpawnSpec, SubprocessHandle};
use serde::{Deserialize, Serialize};
use std::{
    io::{self, Read, Write},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    runtime::Handle as Runtime,
};
use vm_rfb::{IO_TIMEOUT, RfbDisplay};

const LAUNCHER: &str = include_str!("launch.py");
const DIAGNOSTIC_CAP: usize = 16 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct XEnvironment {
    pub display: String,
    pub xauthority: String,
}
impl XEnvironment {
    pub fn variables(&self) -> Vec<(String, String)> {
        vec![("DISPLAY".into(), self.display.clone()), ("XAUTHORITY".into(), self.xauthority.clone())]
    }
}

pub struct XServerDisplay {
    process: SubprocessHandle,
    guard: Guard,
    width: u16,
    height: u16,
    instance: Mutex<Option<Instance>>,
}

struct Instance {
    display: RfbDisplay,
    environment: XEnvironment,
    child: Arc<dyn Child>,
    runtime: Runtime,
    guard: Guard,
    diagnostics: tokio::task::JoinHandle<()>,
}

impl Drop for Instance {
    fn drop(&mut self) {
        self.diagnostics.abort();
        // Closing RFB stdin also causes the world-side launcher to reap both servers.
        // An expired VM is reaped by its lifecycle manager, not by a revoked handle.
        if (self.guard)().is_ok() {
            let child = self.child.clone();
            let guard = self.guard.clone();
            self.runtime.spawn(async move {
                if guard().is_ok() {
                    let _ = child.kill().await;
                }
            });
        }
    }
}

impl XServerDisplay {
    /// Does not start a process until the first operation. Must be called on a blocking
    /// worker entered in a live Tokio runtime; the provided Subprocess is the sole authority.
    pub fn new(process: SubprocessHandle, guard: Guard, width: u16, height: u16) -> Result<Self, DisplayError> {
        if width == 0
            || height == 0
            || width > vm_rfb::MAX_DIMENSION
            || height > vm_rfb::MAX_DIMENSION
            || usize::from(width) * usize::from(height) > vm_rfb::MAX_PIXELS
        {
            return Err(DisplayError::BadArgs("X desktop dimensions exceed framebuffer bounds".into()));
        }
        Ok(Self { process, guard, width, height, instance: Mutex::new(None) })
    }

    fn with<T>(&self, operation: impl FnOnce(&Instance) -> Result<T, DisplayError>) -> Result<T, DisplayError> {
        (self.guard)().map_err(DisplayError::Denied)?;
        let mut instance = self.instance.lock().map_err(|_| DisplayError::Io("X desktop lock poisoned".into()))?;
        if instance.is_none() {
            *instance = Some(self.start()?);
        }
        let instance = instance.as_ref().ok_or_else(|| DisplayError::Io("X desktop failed to initialize".into()))?;
        operation(instance)
    }

    /// Start if necessary and report the environment for xterm or any other X client.
    /// Paths and display belong to the bound world, not the host running aho.
    pub fn environment(&self) -> Result<XEnvironment, DisplayError> {
        self.with(|i| Ok(i.environment.clone()))
    }

    pub fn close(&self) -> Result<(), DisplayError> {
        (self.guard)().map_err(DisplayError::Denied)?;
        let instance = self.instance.lock().map_err(|_| DisplayError::Io("X desktop lock poisoned".into()))?.take();
        if let Some(instance) = instance {
            instance.runtime.block_on(instance.child.kill()).map_err(|e| DisplayError::Io(e.to_string()))?;
        }
        Ok(())
    }

    fn start(&self) -> Result<Instance, DisplayError> {
        let runtime = Runtime::try_current()
            .map_err(|_| DisplayError::Unsupported("X desktop requires a live Tokio runtime and a blocking worker".into()))?;
        (self.guard)().map_err(DisplayError::Denied)?;
        let transport = runtime.block_on(self.process.0.spawn_transport(SpawnSpec {
            argv: vec!["python3".into(), "-u".into(), "-c".into(), LAUNCHER.into(), self.width.to_string(), self.height.to_string()],
            ..SpawnSpec::default()
        })).map_err(|e| DisplayError::Unsupported(format!("cannot launch session desktop in the bound world: {e}; requires separate protocol streams and python3, Xvfb, x11vnc installed there (never falls back to host)")))?;
        let mut stderr = transport.stderr;
        let diagnostics = Arc::new(Mutex::new(Vec::new()));
        let captured = diagnostics.clone();
        let guard = self.guard.clone();
        let drain = runtime.spawn(async move {
            let mut chunk = [0; 4096];
            loop {
                if guard().is_err() {
                    break;
                }
                match stderr.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let Ok(mut output) = captured.lock() else { break };
                        let keep = n.min(DIAGNOSTIC_CAP.saturating_sub(output.len()));
                        output.extend_from_slice(&chunk[..keep]);
                    }
                }
            }
        });
        let mut reader = WorldReader { inner: transport.stdout, runtime: runtime.clone() };
        let writer = WorldWriter { inner: transport.stdin, runtime: runtime.clone() };
        let initialized = (|| {
            let mut line = Vec::new();
            loop {
                (self.guard)().map_err(DisplayError::Denied)?;
                if line.len() >= 4096 {
                    return Err(DisplayError::Protocol("X server readiness line exceeds 4096 bytes".into()));
                }
                let mut byte = [0];
                reader.read_exact(&mut byte).map_err(|e| DisplayError::Io(format!("X server startup: {e}")))?;
                if byte[0] == b'\n' {
                    break;
                }
                line.push(byte[0]);
            }
            let environment = serde_json::from_slice(&line).map_err(|e| DisplayError::Protocol(format!("X server readiness: {e}")))?;
            let display = RfbDisplay::from_io(reader, writer, self.guard.clone())?;
            Ok((display, environment))
        })();
        match initialized {
            Ok((display, environment)) => {
                Ok(Instance { display, environment, child: transport.child, runtime, guard: self.guard.clone(), diagnostics: drain })
            }
            Err(error) => {
                if (self.guard)().is_ok() {
                    let _ = runtime.block_on(transport.child.kill());
                }
                // Let the diagnostics pipe report a missing binary/startup error before rendering it.
                runtime.block_on(async {
                    let _ = tokio::time::timeout(Duration::from_millis(250), &mut Box::pin(drain)).await;
                });
                let text = diagnostics.lock().map(|v| String::from_utf8_lossy(&v).into_owned()).unwrap_or_default();
                Err(DisplayError::Io(format!("{error}. Requires python3, Xvfb and x11vnc inside the bound world. {text}")))
            }
        }
    }
}

impl Desktop for XServerDisplay {
    fn screenshot(&self, request: &CaptureRequest) -> Result<Screenshot, DisplayError> {
        self.with(|i| i.display.screenshot(request))
    }
    fn key(&self, event: KeyEvent) -> Result<(), DisplayError> {
        self.with(|i| i.display.key(event))
    }
    fn pointer(&self, event: PointerEvent) -> Result<(), DisplayError> {
        self.with(|i| i.display.pointer(event))
    }
    fn state(&self) -> Result<PointerState, DisplayError> {
        self.with(|i| i.display.state())
    }
}

struct WorldReader {
    inner: Box<dyn AsyncRead + Unpin + Send>,
    runtime: Runtime,
}
impl Read for WorldReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.runtime.block_on(async {
            tokio::time::timeout(IO_TIMEOUT, self.inner.read(bytes))
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "world RFB read timed out"))?
        })
    }
}
struct WorldWriter {
    inner: Box<dyn AsyncWrite + Unpin + Send>,
    runtime: Runtime,
}
impl Write for WorldWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.runtime.block_on(async {
            tokio::time::timeout(IO_TIMEOUT, self.inner.write(bytes))
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "world RFB write timed out"))?
        })
    }
    fn flush(&mut self) -> io::Result<()> {
        self.runtime.block_on(async {
            tokio::time::timeout(IO_TIMEOUT, self.inner.flush())
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "world RFB flush timed out"))?
        })
    }
}

pub struct XServerDeps {
    pub process: SubprocessHandle,
    pub authority: DesktopGuardHandle,
}
impl Deps for XServerDeps {
    const REQUIRES: &'static [HandleKind] = &[HandleKind::Subprocess, HandleKind::Custom("desktop-guard")];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(Self { process: r.handle()?, authority: r.handle()? })
    }
}

pub struct DisplayXserver {
    display: Arc<XServerDisplay>,
}
impl Capability for DisplayXserver {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = XServerDeps;
    type Config = ();
    fn build(deps: XServerDeps, _: ()) -> Result<Self, BuildError> {
        let display = XServerDisplay::new(deps.process, deps.authority.0, 1024, 768).map_err(|e| BuildError::Probe(e.to_string()))?;
        Ok(Self { display: Arc::new(display) })
    }
    fn register(&self, r: &mut Registrar<'_>) {
        r.provide(DesktopHandle(self.display.clone()));
    }
}

declare_capability!(
    DisplayXserver,
    "display-xserver",
    kind = Kind::Provider,
    provides = [aho_display::SEAM],
    requires = [Requirement::Seam(aho_proc::SEAM)],
    tools = &[],
    provides_handles = [HandleKind::Desktop],
    enforced_by = "bound world's Subprocess and session lifecycle guard; no host fallback",
    doc = "A session-private Xvfb server plus x11vnc over a private Unix socketpair, relayed through the bound world's protocol process streams. Requires python3, Xvfb and x11vnc inside a POSIX world, checked at launch rather than incorrectly probing host binaries. Host means policy only; namespace and VM preserve their own process boundary. X clients must use this desktop's DISPLAY and XAUTHORITY. Windows worlds are unsupported."
);
