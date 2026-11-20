//! Control the Whisplay display through a local hardware daemon.
//!
//! [`Client`] serves one logical owner with at most one active animation.
//! Disconnecting does not stop playback. The optional `daemon` feature builds
//! `whisplayd`; client-only applications can leave it off.
//!
//! # Acceptance and errors
//!
//! Display submissions return after the daemon owns their input, before LCD
//! transfer completion. Callers may then reuse pixel slices or remove source files.
//! Initial validation errors are returned before replacing the current display
//! when possible. Later decoding/output failures are logged to daemon stderr and
//! retained in [`State`]. A failed SPI transaction can partially update the panel.
//! There is no transfer-completion or physical-scanout notification.
//!
//! # Example
//!
//! ```no_run
//! use std::path::Path;
//! use whisplay_driver::{Client, DEFAULT_SOCKET, HEIGHT, WIDTH};
//!
//! # fn main() -> std::io::Result<()> {
//! let display = Client::new(Path::new(DEFAULT_SOCKET));
//! display.display_file(Path::new("animation.gif"), 0..WIDTH, 0..HEIGHT)?;
//! display.clear()?;
//! # Ok(())
//! # }
//! ```

use serde::{Deserialize, Serialize};
use serde_jsonlines::JsonLinesWriter;
use std::{
    fs::OpenOptions,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    ops::Range,
    os::unix::{fs::OpenOptionsExt, net::UnixStream},
    path::{Path, PathBuf},
    time::Duration,
};
use tempfile::NamedTempFile;

/// LCD width in pixels.
pub const WIDTH: u16 = 240;
/// LCD height in pixels.
pub const HEIGHT: u16 = 280;
/// Socket used by the daemon and CLI unless explicitly overridden.
pub const DEFAULT_SOCKET: &str = "/tmp/whisplay-rs.sock";
const MAX_HEADER: usize = 8192;

/// The display's current presentation state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayState {
    /// Initialized or explicitly cleared to black.
    Blank,
    /// Holding an image or the final frame of a completed animation.
    Static,
    /// An animation is active, including delays between frames.
    Playing,
    /// Playback or output failed; previous or partially written pixels may remain.
    Failed,
}

/// Current presentation state and the latest playback failure.
/// This is not a transfer-completion or physical scanout signal.
/// Rejected submissions, state queries and brightness/LED changes preserve errors.
#[derive(Debug, Serialize, Deserialize)]
pub struct State {
    /// Current presentation state, including time between animation frames.
    pub state: DisplayState,
    /// Retained until another file, pixel or clear submission is accepted.
    pub error: Option<String>,
}

/// One logical owner's connection settings and reusable, private upload storage.
///
/// Column/row ranges are nonempty and half-open, measured from the top left.
/// Full screen is `0..WIDTH, 0..HEIGHT`; reversed or out-of-screen ranges fail.
/// Display submissions write only the selected area and retain surrounding pixels.
#[derive(Debug)]
pub struct Client {
    socket: PathBuf,
    pixel_upload: Option<NamedTempFile>,
}

impl Client {
    /// Store connection settings without contacting the daemon.
    /// Connection and validation errors are returned by each submission.
    pub fn new(socket: &Path) -> Self {
        Self {
            socket: socket.into(),
            pixel_upload: None,
        }
    }

    /// Fit a local PNG, JPEG or GIF inside the selected area, replacing playback.
    ///
    /// Preserve aspect ratio, center on black padding, and composite transparency
    /// over black. Regions follow the coordinate rules on [`Client`].
    /// Files must be regular files, at most 16 MiB encoded, with a decoded canvas
    /// of at most 4 million pixels. Additional decoder working-buffer limits apply;
    /// these are not a total process-memory limit. EXIF rotation is not applied.
    ///
    /// GIF delays and loop counts come from the file; absent loop information means
    /// one pass, with the last frame retained. Zero-delay frames use a 1 ms minimum.
    /// Timing is best effort: obsolete intermediate LCD writes may be skipped.
    /// Frames are decoded during playback and may be cached for subsequent loops.
    /// Acceptance does not wait for a whole animation to decode; see the
    /// [crate-level acceptance contract](crate#acceptance-and-errors).
    pub fn display_file(
        &self,
        source: &Path,
        columns: Range<u16>,
        rows: Range<u16>,
    ) -> io::Result<()> {
        pixel_bytes(&columns, &rows)?;
        let path = source.canonicalize()?;
        self.submit(Request::DisplayFile {
            path,
            columns,
            rows,
        })
    }

    /// Submit pixels for the selected area, cancelling animation.
    ///
    /// Pixels are packed row-major RGB565, big-endian, without row padding or
    /// resizing: exactly area width × height × 2 bytes. Regions follow [`Client`].
    /// Reusable private `/dev/shm` storage stages a copy; the daemon owns a separate
    /// snapshot before acceptance. The caller may then reuse the slice.
    pub fn display_pixels(
        &mut self,
        pixels: &[u8],
        columns: Range<u16>,
        rows: Range<u16>,
    ) -> io::Result<()> {
        if pixels.len() != pixel_bytes(&columns, &rows)? {
            return Err(invalid("pixel length does not match the selected area"));
        }
        let upload = match &mut self.pixel_upload {
            Some(upload) => upload,
            empty => empty.insert(
                tempfile::Builder::new()
                    .prefix("whisplay-")
                    .tempfile_in("/dev/shm")?,
            ),
        };
        upload.seek(SeekFrom::Start(0))?;
        upload.as_file().set_len(pixels.len() as u64)?;
        upload.write_all(pixels)?;
        let path = upload.path().to_owned();
        self.submit(Request::DisplayPixels {
            path,
            columns,
            rows,
        })
    }

    /// Cancel previous playback and accept a full-screen black update.
    ///
    /// Earlier accepted work cannot write after this acknowledgment. Brightness is
    /// unchanged. The black transfer follows the same
    /// [acceptance contract](crate#acceptance-and-errors) as other display requests.
    pub fn clear(&self) -> io::Result<()> {
        self.submit(Request::Clear)
    }

    /// Set LCD screen brightness: 0 is off, 100 is full brightness.
    /// No automatic dimming policy.
    pub fn set_backlight(&self, percent: u8) -> io::Result<()> {
        if percent > 100 {
            return Err(invalid("backlight must be in 0..=100"));
        }
        self.submit(Request::Backlight { percent })
    }

    /// Set the board's RGB indicator light color, independently of the LCD.
    /// Channels are [red, green, blue], each 0 (off) through 255 (full brightness).
    /// [255, 0, 0] is red; [0, 0, 0] turns the indicator light off. No fades.
    pub fn set_led(&self, rgb: [u8; 3]) -> io::Result<()> {
        self.submit(Request::Led { rgb })
    }

    /// Read playback state without clearing its error or changing the display.
    ///
    /// Queries wait behind input preparation or transfers already in progress.
    /// This snapshot does not confirm transfer completion or physical scanout.
    pub fn state(&self) -> io::Result<State> {
        match self.request(Request::State)? {
            Reply::State(state) => Ok(state),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected playback state",
            )),
        }
    }

    fn submit(&self, request: Request) -> io::Result<()> {
        match self.request(request)? {
            Reply::Ok => Ok(()),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected acceptance reply",
            )),
        }
    }

    fn request(&self, request: Request) -> io::Result<Reply> {
        let mut stream = UnixStream::connect(&self.socket)?;
        let timeout = Some(Duration::from_secs(30));
        stream.set_read_timeout(timeout)?;
        stream.set_write_timeout(timeout)?;
        JsonLinesWriter::new(&mut stream).write(&request)?;
        match read_message::<Reply>(&mut BufReader::new(stream))? {
            Reply::Error { message } => Err(io::Error::other(message)),
            reply => Ok(reply),
        }
    }
}

/// Internal protocol types and input helpers shared by the client, daemon and presentation adapter.
#[doc(hidden)]
pub mod shared {
    use super::*;

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
    pub enum Request {
        DisplayFile {
            path: PathBuf,
            columns: Range<u16>,
            rows: Range<u16>,
        },
        DisplayPixels {
            path: PathBuf,
            columns: Range<u16>,
            rows: Range<u16>,
        },
        Clear,
        Backlight {
            percent: u8,
        },
        Led {
            rgb: [u8; 3],
        },
        State,
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(tag = "result", rename_all = "snake_case")]
    pub enum Reply {
        Ok,
        State(State),
        Error { message: String },
    }

    pub fn read_message<T: serde::de::DeserializeOwned>(
        reader: &mut impl BufRead,
    ) -> io::Result<T> {
        let mut reader =
            serde_jsonlines::JsonLinesReader::new(reader.take((MAX_HEADER + 1) as u64));
        let message = reader.read()?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::UnexpectedEof, "missing control message")
        })?;
        if reader.get_ref().limit() == 0 {
            return Err(invalid("oversized control message"));
        }
        Ok(message)
    }

    pub fn pixel_bytes(columns: &Range<u16>, rows: &Range<u16>) -> io::Result<usize> {
        if columns.start >= columns.end
            || columns.end > WIDTH
            || rows.start >= rows.end
            || rows.end > HEIGHT
        {
            return Err(invalid(
                "pixel ranges must be nonempty and inside 240 x 280",
            ));
        }
        Ok(usize::from(columns.end - columns.start) * usize::from(rows.end - rows.start) * 2)
    }

    pub fn invalid(message: impl std::fmt::Display) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidInput, message.to_string())
    }

    /// Own a finite snapshot. An open descriptor alone does not freeze input.
    pub fn snapshot(path: &Path, maximum: usize) -> io::Result<Vec<u8>> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)?;
        if !file.metadata()?.is_file() {
            return Err(invalid("input must be a regular file"));
        }
        let mut bytes = Vec::new();
        file.take(maximum as u64 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > maximum {
            return Err(invalid("input exceeds size limit"));
        }
        Ok(bytes)
    }
}

use shared::{invalid, pixel_bytes, read_message, Reply, Request};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_ranges_reject_empty_reversed_and_outside_areas() {
        for (columns, rows) in [
            (1..1, 0..1),
            (Range { start: 2, end: 1 }, 0..1),
            (0..241, 0..1),
            (0..1, 0..281),
        ] {
            assert!(pixel_bytes(&columns, &rows).is_err());
        }
        assert_eq!(pixel_bytes(&(20..80), &(40..70)).unwrap(), 3600);
    }

    #[test]
    fn client_reuses_shared_storage_and_cleans_it_up_after_acknowledgement() {
        use std::{os::unix::net::UnixListener, thread};
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("display.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let mut snapshots = Vec::new();
            let mut paths = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let request: Request = read_message(&mut BufReader::new(&stream)).unwrap();
                match request {
                    Request::DisplayPixels {
                        path,
                        columns,
                        rows,
                    } => {
                        snapshots.push(
                            shared::snapshot(&path, pixel_bytes(&columns, &rows).unwrap()).unwrap(),
                        );
                        paths.push(path);
                    }
                    _ => panic!("unexpected command"),
                }
                JsonLinesWriter::new(&mut stream).write(&Reply::Ok).unwrap();
            }
            (snapshots, paths)
        });
        let mut client = Client::new(&socket);
        client.display_pixels(&[0xf8, 0], 0..1, 0..1).unwrap();
        client
            .display_pixels(&[0x07, 0xe0], 239..240, 279..280)
            .unwrap();
        let (snapshots, paths) = server.join().unwrap();
        assert_eq!(snapshots, [vec![0xf8, 0], vec![0x07, 0xe0]]);
        assert_eq!(paths[0], paths[1]);
        assert!(paths[0].exists());
        drop(client);
        assert!(!paths[0].exists());
    }

    #[test]
    fn control_messages_are_bounded_and_reject_incomplete_json() {
        assert!(read_message::<Request>(&mut &b"{\"command\":"[..]).is_err());
        assert!(read_message::<Request>(&mut vec![b'x'; MAX_HEADER + 1].as_slice()).is_err());
        assert!(matches!(
            read_message::<Request>(&mut &b"{\"command\":\"clear\"}\n"[..]).unwrap(),
            Request::Clear
        ));
    }
}
