use crate::{
    Guest,
    protocol::{PIPE_CHUNK, field},
};
use aho_types::BoxFuture;
use serde_json::json;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

fn io_error(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

/// One outstanding `proc.read` of a pipe: the bytes, and whether the pipe ended.
type PendingRead = BoxFuture<'static, io::Result<(Vec<u8>, bool)>>;

pub(crate) struct Reader {
    guest: Arc<Guest>,
    id: u64,
    stream: &'static str,
    pending: Option<PendingRead>,
    bytes: Vec<u8>,
    offset: usize,
    eof: bool,
    failure: Option<String>,
}
impl Reader {
    pub(crate) fn new(guest: Arc<Guest>, id: u64, stream: &'static str) -> Self {
        Self { guest, id, stream, pending: None, bytes: Vec::new(), offset: 0, eof: false, failure: None }
    }
}
impl AsyncRead for Reader {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, target: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        if let Err(e) = self.guest.check() {
            return Poll::Ready(Err(io_error(e.message)));
        }
        if let Some(e) = &self.failure {
            return Poll::Ready(Err(io_error(e.clone())));
        }
        if target.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            if self.offset < self.bytes.len() {
                let n = target.remaining().min(self.bytes.len() - self.offset);
                target.put_slice(&self.bytes[self.offset..self.offset + n]);
                self.offset += n;
                return Poll::Ready(Ok(()));
            }
            if self.eof {
                return Poll::Ready(Ok(()));
            }
            if self.pending.is_none() {
                let (guest, id, stream) = (self.guest.clone(), self.id, self.stream);
                self.pending = Some(Box::pin(async move {
                    loop {
                        let v = guest.call("pipe.read", json!({"id":id,"stream":stream})).await.map_err(|e| io_error(e.message))?;
                        let bytes: Vec<u8> = field(&v, "bytes").map_err(|e| io_error(e.message))?;
                        let eof: bool = field(&v, "eof").map_err(|e| io_error(e.message))?;
                        if bytes.len() > PIPE_CHUNK {
                            return Err(io_error("guest pipe chunk exceeds protocol limit"));
                        }
                        if !bytes.is_empty() || eof {
                            return Ok((bytes, eof));
                        }
                    }
                }));
            }
            let result = match self.pending.as_mut() {
                Some(f) => f.as_mut().poll(cx),
                None => unreachable!(),
            };
            match result {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(result) => {
                    self.pending = None;
                    match result {
                        Ok((bytes, eof)) => {
                            self.bytes = bytes;
                            self.offset = 0;
                            self.eof = eof;
                        }
                        Err(e) => {
                            self.failure = Some(e.to_string());
                            return Poll::Ready(Err(e));
                        }
                    }
                }
            }
        }
    }
}
impl Drop for Reader {
    fn drop(&mut self) {
        self.guest.release(if self.stream == "stdout" { "pipe.stdout_close" } else { "pipe.stderr_close" }, self.id);
    }
}

pub(crate) struct Writer {
    guest: Arc<Guest>,
    id: u64,
    pending: Option<BoxFuture<'static, io::Result<usize>>>,
    shutdown: Option<BoxFuture<'static, io::Result<()>>>,
    closed: bool,
    failure: Option<String>,
}
impl Writer {
    pub(crate) fn new(guest: Arc<Guest>, id: u64) -> Self {
        Self { guest, id, pending: None, shutdown: None, closed: false, failure: None }
    }
    fn poll_pending(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let result = match self.pending.as_mut() {
            Some(f) => f.as_mut().poll(cx),
            None => return Poll::Ready(Ok(0)),
        };
        if let Poll::Ready(result) = result {
            self.pending = None;
            if let Err(e) = &result {
                self.failure = Some(e.to_string());
            }
            Poll::Ready(result)
        } else {
            Poll::Pending
        }
    }
}
impl AsyncWrite for Writer {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, bytes: &[u8]) -> Poll<io::Result<usize>> {
        if let Err(e) = self.guest.check() {
            return Poll::Ready(Err(io_error(e.message)));
        }
        if let Some(e) = &self.failure {
            return Poll::Ready(Err(io_error(e.clone())));
        }
        if self.closed || self.shutdown.is_some() {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        if self.pending.is_none() {
            let n = bytes.len().min(PIPE_CHUNK);
            if n == 0 {
                return Poll::Ready(Ok(0));
            }
            let (guest, id) = (self.guest.clone(), self.id);
            let args = json!({"id":id,"bytes":&bytes[..n]});
            self.pending = Some(Box::pin(async move {
                guest.call("pipe.write", args).await.map_err(|e| io_error(e.message))?;
                Ok(n)
            }));
        }
        self.poll_pending(cx)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Err(e) = self.guest.check() {
            return Poll::Ready(Err(io_error(e.message)));
        }
        if let Some(e) = &self.failure {
            return Poll::Ready(Err(io_error(e.clone())));
        }
        self.poll_pending(cx).map(|r| r.map(|_| ()))
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Err(e) = self.guest.check() {
            return Poll::Ready(Err(io_error(e.message)));
        }
        match self.poll_pending(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Ready(Ok(_)) => {}
        }
        if self.closed {
            return Poll::Ready(Ok(()));
        }
        if self.shutdown.is_none() {
            let (guest, id) = (self.guest.clone(), self.id);
            self.shutdown = Some(Box::pin(async move {
                guest.call("pipe.stdin_close", json!({"id":id})).await.map(|_| ()).map_err(|e| io_error(e.message))
            }));
        }
        let result = match self.shutdown.as_mut() {
            Some(f) => f.as_mut().poll(cx),
            None => unreachable!(),
        };
        if result.is_ready() {
            self.shutdown = None;
            self.closed = true;
        }
        result
    }
}
impl Drop for Writer {
    fn drop(&mut self) {
        if !self.closed {
            self.guest.release("pipe.stdin_close", self.id);
        }
    }
}
