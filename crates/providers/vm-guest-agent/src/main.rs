#![forbid(unsafe_code)]

mod service;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Semaphore, mpsc};
use vm_guest::protocol::{DEVICE, MAX_FRAME, MAX_INFLIGHT, Request, Response, WireError};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("vm-guest-agent: {error}");
        std::process::exit(1);
    }
}

#[allow(clippy::disallowed_methods, reason = "authority: vm-guest-agent-endpoint")]
async fn run() -> Result<(), String> {
    let mut workspace = None;
    let mut socket = None;
    let mut device = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--workspace" => workspace = Some(args.next().ok_or("--workspace needs an absolute path")?),
            "--socket" => socket = Some(PathBuf::from(args.next().ok_or("--socket needs a path")?)),
            "--device" => device = Some(PathBuf::from(args.next().ok_or("--device needs a path")?)),
            "--help" | "-h" => {
                println!("vm-guest-agent --workspace ABSOLUTE_DIR [--device /dev/virtio-ports/org.aho.agent | --socket UNIX_PATH]");
                return Ok(());
            }
            _ => return Err(format!("unknown argument `{arg}`; use --help")),
        }
    }
    let workspace =
        workspace.ok_or("--workspace ABSOLUTE_DIR is required; the agent never chooses the host/current directory implicitly")?;
    if !workspace.starts_with('/') {
        return Err("--workspace must be absolute".into());
    }
    if socket.is_some() && device.is_some() {
        return Err("--socket and --device are mutually exclusive".into());
    }
    let server = Arc::new(service::Server::new(workspace)?);
    if let Some(socket) = socket {
        let listener = tokio::net::UnixListener::bind(&socket)
            .map_err(|e| format!("cannot bind fixture socket `{}`: {e}; choose an unused socket path", socket.display()))?;
        loop {
            let (stream, _) = listener.accept().await.map_err(|e| e.to_string())?;
            let server = server.clone();
            tokio::spawn(async move {
                let (r, w) = stream.into_split();
                if let Err(e) = serve(server, r, w).await {
                    eprintln!("guest channel: {e}");
                }
            });
        }
    }
    let device = device.unwrap_or_else(|| PathBuf::from(DEVICE));
    loop {
        // Separate file handles allow a pending virtio read and a reply write simultaneously.
        let reader = tokio::fs::OpenOptions::new().read(true).write(true).open(&device).await.map_err(|e| {
            format!("cannot open virtio port `{}`: {e}; configure org.aho.agent in QEMU and load virtio_console", device.display())
        })?;
        let writer = reader.try_clone().await.map_err(|e| e.to_string())?;
        if let Err(e) = serve(server.clone(), reader, writer).await {
            eprintln!("guest channel: {e}; waiting for host reconnect");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn serve<R, W>(server: Arc<service::Server>, mut reader: R, mut writer: W) -> Result<(), String>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (tx, mut rx) = mpsc::channel::<Response>(MAX_INFLIGHT);
    let write_task = tokio::spawn(async move {
        while let Some(mut response) = rx.recv().await {
            let mut bytes = serde_json::to_vec(&response).map_err(|e| e.to_string())?;
            if bytes.len() > MAX_FRAME {
                response.result =
                    Err(WireError::protocol("guest result exceeds 64 MiB frame limit; narrow the read/range or split the operation"));
                bytes = serde_json::to_vec(&response).map_err(|e| e.to_string())?;
            }
            let write = async {
                writer.write_u32(bytes.len() as u32).await?;
                writer.write_all(&bytes).await?;
                writer.flush().await
            };
            tokio::time::timeout(Duration::from_secs(25), write)
                .await
                .map_err(|_| "guest reply write timed out".to_string())?
                .map_err(|e| e.to_string())?;
        }
        Ok::<_, String>(())
    });
    let limit = Arc::new(Semaphore::new(MAX_INFLIGHT));
    let mut requests = tokio::task::JoinSet::new();
    let result = loop {
        let size = match reader.read_u32().await {
            Ok(n) => n as usize,
            Err(e) => break Err(e.to_string()),
        };
        if size == 0 || size > MAX_FRAME {
            break Err("invalid request frame length".into());
        }
        let mut body = vec![0; size];
        if let Err(e) = tokio::time::timeout(Duration::from_secs(25), reader.read_exact(&mut body)).await {
            break Err(format!("incomplete request frame: {e}"));
        }
        let request: Request = match serde_json::from_slice(&body) {
            Ok(r) => r,
            Err(e) => break Err(format!("invalid request JSON: {e}")),
        };
        let permit = match limit.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                let _ =
                    tx.send(Response { id: request.id, result: Err(WireError::protocol("guest request concurrency limit reached")) }).await;
                continue;
            }
        };
        let (server, tx) = (server.clone(), tx.clone());
        requests.spawn(async move {
            let result = server.dispatch(&request).await;
            let _ = tx.send(Response { id: request.id, result }).await;
            drop(permit);
        });
        while requests.try_join_next().is_some() {}
    };
    // An interrupted effect is never replayed by the host. Keep accepted effects alive through
    // reconnect; their handles remain in the server's session, rather than killing live jobs.
    requests.detach_all();
    write_task.abort();
    result
}
