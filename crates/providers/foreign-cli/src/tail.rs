//! Byte-offset tail of an append-only JSONL transcript (12: learn state from *its* transcript).
//!
//! * The committed offset is always a line boundary: a line is handed out only once its `\n` is
//!   on disk, and the offset moves past it. A partial last line waits, buffered, for the rest.
//! * Resume after a crash = `Tail::resume(path, offset, ident)` with the offset and file identity
//!   recorded in the parent log; nothing before the offset is read again.
//! * A file that got shorter than the offset was truncated, one whose identity (device, inode)
//!   changed was replaced (rotated), and a resume offset that does not sit right after a `\n`
//!   means the bytes under it changed. All three restart from byte 0 and say so in
//!   [`TailRead::reset`]; the caller records it instead of guessing.

use std::fs::File;
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Bytes read per poll at most, so one huge append cannot stall the caller.
pub const READ_CHUNK: u64 = 4 << 20;

/// `(device, inode)` of a file on unix; `None` elsewhere.
pub type FileIdent = Option<(u64, u64)>;

/// Why the tail restarted from byte 0.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reset {
    /// The file is shorter than what was already read.
    Truncated { was: u64, len: u64 },
    /// A different file now lives at the path.
    Replaced,
    /// The resume offset did not sit right after a line break.
    Misaligned { offset: u64 },
}

impl Reset {
    pub fn describe(&self) -> String {
        match self {
            Reset::Truncated { was, len } => format!("the transcript shrank from {was} to {len} bytes; reading it again from the start"),
            Reset::Replaced => "a different transcript file now sits at the same path; reading it from the start".into(),
            Reset::Misaligned { offset } => {
                format!("the saved offset {offset} is not at a line boundary any more; reading the transcript again from the start")
            }
        }
    }
}

/// One complete line. `end` is the offset just past its `\n`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub end: u64,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct TailRead {
    pub reset: Option<Reset>,
    pub lines: Vec<Line>,
    /// False while the file does not exist yet.
    pub exists: bool,
}

#[derive(Debug)]
pub struct Tail {
    path: PathBuf,
    offset: u64,
    pending: Vec<u8>,
    ident: FileIdent,
    /// A resumed offset is checked against the file once, on the first poll.
    verify: bool,
}

fn ident_of(meta: &std::fs::Metadata) -> FileIdent {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some((meta.dev(), meta.ino()))
    }
    #[cfg(not(unix))]
    {
        let _unused = meta;
        None
    }
}

/// The identity of the file at `path` now (`None` when missing or not on unix).
#[allow(clippy::disallowed_methods, reason = "authority: foreign-cli-transcript-identity")]
pub fn file_ident(path: &Path) -> FileIdent {
    std::fs::metadata(path).ok().and_then(|m| ident_of(&m))
}

impl Tail {
    /// A tail from the start of `path`.
    pub fn new(path: impl Into<PathBuf>) -> Tail {
        Tail { path: path.into(), offset: 0, pending: Vec::new(), ident: None, verify: false }
    }

    /// A tail that continues at `offset` of the file identified by `ident`.
    pub fn resume(path: impl Into<PathBuf>, offset: u64, ident: FileIdent) -> Tail {
        Tail { path: path.into(), offset, pending: Vec::new(), ident, verify: offset > 0 }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    /// Committed offset: everything before it has been handed out as complete lines.
    pub fn offset(&self) -> u64 {
        self.offset
    }
    pub fn ident(&self) -> FileIdent {
        self.ident
    }

    fn restart(&mut self, why: Reset, read: &mut TailRead) {
        self.offset = 0;
        self.pending.clear();
        self.verify = false;
        read.reset = Some(why);
    }

    /// Reads the complete lines appended since the last poll. A missing file is not an error
    /// (`exists: false`): the CLI writes its transcript lazily.
    #[allow(clippy::disallowed_methods, reason = "authority: foreign-cli-transcript-read")]
    pub fn poll(&mut self) -> std::io::Result<TailRead> {
        let mut read = TailRead::default();
        let mut file = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(read),
            Err(e) => return Err(e),
        };
        read.exists = true;
        let meta = file.metadata()?;
        let len = meta.len();
        let ident = ident_of(&meta);
        let consumed = self.offset + self.pending.len() as u64;
        if self.ident.is_some() && ident.is_some() && self.ident != ident {
            self.restart(Reset::Replaced, &mut read);
        } else if len < consumed {
            self.restart(Reset::Truncated { was: consumed, len }, &mut read);
        } else if self.verify {
            self.verify = false;
            file.seek(SeekFrom::Start(self.offset - 1))?;
            let mut b = [0u8; 1];
            file.read_exact(&mut b)?;
            if b[0] != b'\n' {
                let offset = self.offset;
                self.restart(Reset::Misaligned { offset }, &mut read);
            }
        }
        self.ident = ident;

        let from = self.offset + self.pending.len() as u64;
        if len <= from {
            return Ok(read);
        }
        file.seek(SeekFrom::Start(from))?;
        let want = (len - from).min(READ_CHUNK);
        let start = self.pending.len();
        let mut chunk = Vec::with_capacity(usize::try_from(want).unwrap_or(0));
        (&mut file).take(want).read_to_end(&mut chunk)?;
        self.pending.extend_from_slice(&chunk);

        let mut line_start = 0usize;
        let mut scan = start;
        while let Some(i) = self.pending[scan..].iter().position(|b| *b == b'\n') {
            let nl = scan + i;
            let mut bytes = self.pending[line_start..nl].to_vec();
            if bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
            let end = self.offset + nl as u64 + 1;
            if !bytes.iter().all(u8::is_ascii_whitespace) {
                read.lines.push(Line { end, bytes });
            }
            line_start = nl + 1;
            scan = nl + 1;
        }
        if line_start > 0 {
            self.offset += line_start as u64;
            self.pending.drain(..line_start);
        }
        Ok(read)
    }
}
