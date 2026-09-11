//! RFB 3.8 with None security on an already-authorized local transport, raw true-color
//! pixels, DesktopSize, LastRect and PointerPos. No TCP listener or host desktop fallback.
#![forbid(unsafe_code)]

use aho_display::{CaptureRequest, Desktop, DisplayError, Guard, KeyEvent, PointerEvent, PointerState, Screenshot};
use std::{
    io::{Read, Write},
    path::Path,
    sync::{Mutex, MutexGuard},
    time::{Duration, Instant},
};

pub const MAX_PIXELS: usize = 16_777_216;
pub const MAX_DIMENSION: u16 = 8192;
pub const IO_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_TEXT: usize = 1 << 20;
const MAX_MESSAGES: usize = 4096;

pub struct RfbDisplay {
    client: Mutex<Client>,
}

struct Client {
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    guard: Guard,
    deadline: Instant,
    state: PointerState,
    pixels: Vec<u8>,
    broken: bool,
}

impl RfbDisplay {
    /// The manager owns socket selection and the guard (normally a VM lease check).
    #[cfg(unix)]
    #[allow(clippy::disallowed_methods, reason = "authority: vm-rfb-connect")]
    pub fn connect(path: impl AsRef<Path>, guard: Guard) -> Result<Self, DisplayError> {
        guard().map_err(DisplayError::Denied)?;
        let socket = std::os::unix::net::UnixStream::connect(path.as_ref()).map_err(io_error)?;
        socket.set_read_timeout(Some(IO_TIMEOUT)).map_err(io_error)?;
        socket.set_write_timeout(Some(IO_TIMEOUT)).map_err(io_error)?;
        let writer = socket.try_clone().map_err(io_error)?;
        Self::from_io(socket, writer, guard)
    }

    #[cfg(not(unix))]
    pub fn connect(_: impl AsRef<Path>, _: Guard) -> Result<Self, DisplayError> {
        Err(DisplayError::Unsupported("QEMU RFB Unix sockets require Linux or macOS".into()))
    }

    /// Supplied streams must enforce a finite per-read/write timeout (at most IO_TIMEOUT).
    /// This constructor is also used by display-xserver's bound-world protocol child.
    pub fn from_io(reader: impl Read + Send + 'static, writer: impl Write + Send + 'static, guard: Guard) -> Result<Self, DisplayError> {
        let mut client = Client {
            reader: Box::new(reader),
            writer: Box::new(writer),
            guard,
            deadline: Instant::now() + IO_TIMEOUT,
            state: PointerState { width: 0, height: 0, x: 0, y: 0, buttons: 0, position_known: false },
            pixels: Vec::new(),
            broken: false,
        };
        client.handshake()?;
        Ok(Self { client: Mutex::new(client) })
    }

    fn client(&self) -> Result<MutexGuard<'_, Client>, DisplayError> {
        let mut client = self.client.lock().map_err(|_| DisplayError::Io("RFB connection lock poisoned".into()))?;
        (client.guard)().map_err(DisplayError::Denied)?;
        if client.broken {
            return Err(DisplayError::Io("RFB connection lost synchronization; reconnect the desktop".into()));
        }
        client.deadline = Instant::now() + IO_TIMEOUT;
        Ok(client)
    }
}

impl Client {
    fn check(&mut self) -> Result<(), DisplayError> {
        (self.guard)().map_err(DisplayError::Denied)?;
        if Instant::now() >= self.deadline {
            self.broken = true;
            return Err(DisplayError::Io("RFB operation exceeded ten seconds; reconnect the desktop".into()));
        }
        Ok(())
    }

    fn read(&mut self, bytes: &mut [u8]) -> Result<(), DisplayError> {
        let mut offset = 0;
        while offset < bytes.len() {
            self.check()?;
            match self.reader.read(&mut bytes[offset..]) {
                Ok(0) => {
                    self.broken = true;
                    return Err(DisplayError::Io("RFB peer closed the stream".into()));
                }
                Ok(n) => offset += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    self.broken = true;
                    return Err(io_error(e));
                }
            }
        }
        Ok(())
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), DisplayError> {
        let mut offset = 0;
        while offset < bytes.len() {
            self.check()?;
            match self.writer.write(&bytes[offset..]) {
                Ok(0) => {
                    self.broken = true;
                    return Err(DisplayError::Io("RFB write returned zero".into()));
                }
                Ok(n) => offset += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    self.broken = true;
                    return Err(io_error(e));
                }
            }
        }
        self.check()?;
        self.writer.flush().map_err(|e| {
            self.broken = true;
            io_error(e)
        })
    }

    fn bytes<const N: usize>(&mut self) -> Result<[u8; N], DisplayError> {
        let mut bytes = [0; N];
        self.read(&mut bytes)?;
        Ok(bytes)
    }

    fn text(&mut self) -> Result<String, DisplayError> {
        let len = u32::from_be_bytes(self.bytes()?) as usize;
        if len > MAX_TEXT {
            return self.protocol("RFB text exceeds 1 MiB");
        }
        let mut bytes = vec![0; len];
        self.read(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn protocol<T>(&mut self, message: &str) -> Result<T, DisplayError> {
        self.broken = true;
        Err(DisplayError::Protocol(message.into()))
    }

    fn geometry(&mut self, width: u16, height: u16) -> Result<(), DisplayError> {
        if width == 0
            || height == 0
            || width > MAX_DIMENSION
            || height > MAX_DIMENSION
            || usize::from(width) * usize::from(height) > MAX_PIXELS
        {
            return self.protocol("RFB dimensions exceed 8192 per axis or 16777216 pixels, or are zero");
        }
        self.state.width = width;
        self.state.height = height;
        self.state.x = self.state.x.min(width - 1);
        self.state.y = self.state.y.min(height - 1);
        self.pixels.resize(usize::from(width) * usize::from(height) * 4, 0);
        Ok(())
    }

    fn handshake(&mut self) -> Result<(), DisplayError> {
        if &self.bytes::<12>()? != b"RFB 003.008\n" {
            return self.protocol("RFB 3.8 is required");
        }
        self.write(b"RFB 003.008\n")?;
        let count = self.bytes::<1>()?[0] as usize;
        if count == 0 {
            let reason = self.text()?;
            return self.protocol(&format!("RFB server refused connection: {reason}"));
        }
        let mut methods = [0; 255];
        self.read(&mut methods[..count])?;
        if !methods[..count].contains(&1) {
            return self
                .protocol("RFB None security is required on the authorized local transport; TCP/password authentication is not supported");
        }
        self.write(&[1])?;
        if u32::from_be_bytes(self.bytes()?) != 0 {
            let reason = self.text()?;
            return self.protocol(&format!("RFB security negotiation failed: {reason}"));
        }
        self.write(&[1])?; // Shared ClientInit.
        let header = self.bytes::<20>()?;
        self.geometry(u16::from_be_bytes([header[0], header[1]]), u16::from_be_bytes([header[2], header[3]]))?;
        let _name = self.text()?;
        // SetPixelFormat: 32bpp, depth 24, little-endian true-color B,G,R,padding.
        self.write(&[0, 0, 0, 0, 32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0])?;
        let mut encodings = vec![2, 0, 0, 4];
        for encoding in [0_i32, -223, -224, -232] {
            encodings.extend_from_slice(&encoding.to_be_bytes());
        }
        self.write(&encodings)
    }

    fn request_frame(&mut self) -> Result<(), DisplayError> {
        let mut request = [3, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        request[6..8].copy_from_slice(&self.state.width.to_be_bytes());
        request[8..10].copy_from_slice(&self.state.height.to_be_bytes());
        self.write(&request)
    }

    fn frame(&mut self) -> Result<(), DisplayError> {
        self.request_frame()?;
        // Every request is nonincremental. Coverage avoids returning uninitialized/old pixels,
        // including a DesktopSize-only reply before the server's full resized frame.
        let mut covered = vec![false; usize::from(self.state.width) * usize::from(self.state.height)];
        let mut remaining = covered.len();
        let mut row = Vec::new();
        let mut wire_bytes = 0usize;
        for _ in 0..MAX_MESSAGES {
            match self.bytes::<1>()?[0] {
                0 => {
                    let header = self.bytes::<3>()?;
                    let count = u16::from_be_bytes([header[1], header[2]]);
                    for _ in 0..count {
                        let rectangle = self.bytes::<12>()?;
                        let x = u16::from_be_bytes([rectangle[0], rectangle[1]]) as usize;
                        let y = u16::from_be_bytes([rectangle[2], rectangle[3]]) as usize;
                        let width = u16::from_be_bytes([rectangle[4], rectangle[5]]);
                        let height = u16::from_be_bytes([rectangle[6], rectangle[7]]);
                        let encoding = i32::from_be_bytes([rectangle[8], rectangle[9], rectangle[10], rectangle[11]]);
                        match encoding {
                            -224 => break,
                            -223 => {
                                if x != 0 || y != 0 {
                                    return self.protocol("DesktopSize rectangle has a nonzero origin");
                                }
                                self.geometry(width, height)?;
                                covered.clear();
                                covered.resize(usize::from(width) * usize::from(height), false);
                                remaining = covered.len();
                            }
                            -232 => {
                                if x >= usize::from(self.state.width) || y >= usize::from(self.state.height) {
                                    return self.protocol("RFB cursor is outside framebuffer");
                                }
                                self.state.x = x as u16;
                                self.state.y = y as u16;
                                self.state.position_known = true;
                            }
                            0 => {
                                let stride = usize::from(self.state.width);
                                let width = usize::from(width);
                                let height = usize::from(height);
                                if x + width > stride || y + height > usize::from(self.state.height) {
                                    return self.protocol("RFB raw rectangle is outside framebuffer");
                                }
                                wire_bytes += width * height * 4;
                                if wire_bytes > MAX_PIXELS * 16 {
                                    return self.protocol("RFB update exceeds bounded decode budget");
                                }
                                row.resize(width * 4, 0);
                                for yy in y..y + height {
                                    self.read(&mut row)?;
                                    let start = yy * stride + x;
                                    for (column, pixel) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                                        pixel.swap(0, 2);
                                        pixel[3] = 255;
                                        if !covered[start + column] {
                                            covered[start + column] = true;
                                            remaining -= 1;
                                        }
                                    }
                                    self.pixels[start * 4..(start + width) * 4].copy_from_slice(&row);
                                }
                            }
                            _ => return self.protocol(&format!("RFB server sent unrequested encoding {encoding}")),
                        }
                    }
                    if remaining == 0 {
                        return Ok(());
                    }
                    self.request_frame()?;
                }
                2 => {} // Bell has no body.
                3 => {
                    let _padding = self.bytes::<3>()?;
                    let _clipboard = self.text()?;
                }
                _ => return self.protocol("Unexpected RFB server message"),
            }
        }
        self.protocol("RFB server did not provide a complete framebuffer within the message limit")
    }
}

impl Desktop for RfbDisplay {
    fn screenshot(&self, request: &CaptureRequest) -> Result<Screenshot, DisplayError> {
        if request.max_width == Some(0) || request.max_height == Some(0) {
            return Err(DisplayError::BadArgs("capture bounds must be positive".into()));
        }
        let mut client = self.client()?;
        client.frame()?;
        client.check()?;
        let source_width = u32::from(client.state.width);
        let source_height = u32::from(client.state.height);
        let mut width = source_width.min(request.max_width.unwrap_or(source_width));
        let mut height = ((u64::from(source_height) * u64::from(width)) / u64::from(source_width)).max(1) as u32;
        if height > request.max_height.unwrap_or(source_height) {
            height = request.max_height.unwrap_or(source_height);
            width = ((u64::from(source_width) * u64::from(height)) / u64::from(source_height)).max(1) as u32;
        }
        let mut scaled = Vec::new();
        let pixels = if width == source_width && height == source_height {
            &client.pixels
        } else {
            scaled.resize(width as usize * height as usize * 4, 0);
            for y in 0..height {
                for x in 0..width {
                    let src = ((y as usize * source_height as usize / height as usize) * source_width as usize
                        + x as usize * source_width as usize / width as usize)
                        * 4;
                    let dst = (y as usize * width as usize + x as usize) * 4;
                    scaled[dst..dst + 4].copy_from_slice(&client.pixels[src..src + 4]);
                }
            }
            &scaled
        };
        let mut png = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().map_err(|e| DisplayError::Io(format!("PNG header: {e}")))?;
            writer.write_image_data(pixels).map_err(|e| DisplayError::Io(format!("PNG pixels: {e}")))?;
        }
        Ok(Screenshot { width, height, source_width, source_height, png })
    }

    fn key(&self, event: KeyEvent) -> Result<(), DisplayError> {
        let mut client = self.client()?;
        let mut bytes = [4, u8::from(event.down), 0, 0, 0, 0, 0, 0];
        bytes[4..].copy_from_slice(&event.keysym.to_be_bytes());
        client.write(&bytes)
    }

    fn pointer(&self, event: PointerEvent) -> Result<(), DisplayError> {
        let mut client = self.client()?;
        if event.x >= client.state.width || event.y >= client.state.height {
            return Err(DisplayError::BadArgs(format!(
                "pointer ({},{}) is outside {}x{} framebuffer",
                event.x, event.y, client.state.width, client.state.height
            )));
        }
        let mut bytes = [5, event.buttons, 0, 0, 0, 0];
        bytes[2..4].copy_from_slice(&event.x.to_be_bytes());
        bytes[4..6].copy_from_slice(&event.y.to_be_bytes());
        client.write(&bytes)?;
        client.state.x = event.x;
        client.state.y = event.y;
        client.state.buttons = event.buttons;
        client.state.position_known = true;
        Ok(())
    }

    fn state(&self) -> Result<PointerState, DisplayError> {
        Ok(self.client()?.state)
    }
}

fn io_error(error: std::io::Error) -> DisplayError {
    DisplayError::Io(format!("RFB transport: {error}"))
}
