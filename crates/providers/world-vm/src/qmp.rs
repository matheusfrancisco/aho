//! One bounded QMP transaction per connection, authenticated by the private VM socket directory.
use aho_vm::VmError;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::time::Duration;

const LIMIT: u64 = 1 << 20;
fn io(e: std::io::Error) -> VmError {
    VmError::Io(format!("QMP: {e}"))
}

#[cfg(unix)]
#[allow(clippy::disallowed_methods, reason = "authority: vm-qmp-connect")]
pub fn command(path: &Path, execute: &str, arguments: Value) -> Result<Value, VmError> {
    let stream = std::os::unix::net::UnixStream::connect(path).map_err(io)?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).map_err(io)?;
    stream.set_write_timeout(Some(Duration::from_secs(5))).map_err(io)?;
    let mut reader = BufReader::new(stream);
    let hello = receive(&mut reader)?;
    if hello.get("QMP").is_none() {
        return Err(VmError::Protocol("QEMU did not send a QMP greeting".into()));
    }
    send(reader.get_mut(), "qmp_capabilities", json!({}), 1)?;
    response(&mut reader, 1)?;
    send(reader.get_mut(), execute, arguments, 2)?;
    response(&mut reader, 2)
}

#[cfg(not(unix))]
pub fn command(_: &Path, _: &str, _: Value) -> Result<Value, VmError> {
    Err(VmError::Locked("QEMU world requires Linux or macOS Unix sockets".into()))
}

fn send<W: Write>(writer: &mut W, execute: &str, arguments: Value, id: u64) -> Result<(), VmError> {
    serde_json::to_writer(&mut *writer, &json!({"execute":execute,"arguments":arguments,"id":id}))
        .map_err(|e| VmError::Protocol(e.to_string()))?;
    writer.write_all(b"\r\n").map_err(io)?;
    writer.flush().map_err(io)
}
fn receive<R: BufRead>(reader: &mut R) -> Result<Value, VmError> {
    let mut line = Vec::new();
    reader.take(LIMIT + 1).read_until(b'\n', &mut line).map_err(io)?;
    if line.is_empty() || line.len() as u64 > LIMIT || line.last() != Some(&b'\n') {
        return Err(VmError::Protocol("QMP response exceeded the size limit or ended mid-message".into()));
    }
    serde_json::from_slice(&line).map_err(|e| VmError::Protocol(format!("invalid QMP JSON: {e}")))
}
fn response<R: BufRead>(reader: &mut R, id: u64) -> Result<Value, VmError> {
    for _ in 0..128 {
        let value = receive(reader)?;
        if value.get("id").and_then(Value::as_u64) != Some(id) {
            continue;
        }
        if let Some(error) = value.get("error") {
            return Err(VmError::Protocol(format!("QEMU refused the command: {error}")));
        }
        return value.get("return").cloned().ok_or_else(|| VmError::Protocol("QMP reply has no return value".into()));
    }
    Err(VmError::Protocol("too many QMP events without a matching response".into()))
}

pub fn owned(path: &Path, uuid: &str) -> Result<(), VmError> {
    let v = command(path, "query-uuid", json!({}))?;
    if v.get("UUID").and_then(Value::as_str) != Some(uuid) {
        return Err(VmError::Locked("QMP identity differs from the persisted machine owner; refusing to control it".into()));
    }
    Ok(())
}
