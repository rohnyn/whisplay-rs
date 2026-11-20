//! Own the Whisplay LCD, backlight and RGB LED for local clients.
//!
//! # Running
//!
//! ```sh
//! whisplayd [--socket PATH] [--simulate]
//! ```
//!
//! Hardware mode requires access to `/dev/spidev0.0` and `/dev/gpiochip0` on the Pi.
//! `--simulate` keeps pixels in memory without accessing the board. The socket
//! defaults to [`whisplay_driver::DEFAULT_SOCKET`] and is created with mode 0600.
//! An existing socket path is never automatically replaced. Run the binary directly
//! or arrange service management separately.
//!
//! SIGINT/SIGTERM release the hardware, turn off brightness outputs and remove
//! the socket. See [`whisplay_driver::Client`] for the submission contract.
//!
//! # Sharing the board
//!
//! The supported Raspberry Pi setup uses SPI0/CS0 and BCM GPIO 4, 22, 23, 24, 25,
//! and 27 for display controls and RGB outputs. The button on BCM GPIO 17 and the
//! I2S/I2C audio interfaces are not claimed. Other processes can use those separate
//! functions. A full upstream board-driver instance also requests the display
//! outputs, so use a button-only GPIO reader rather than that instance alongside
//! this daemon.

mod display;
mod hardware;

use display::Display;
use hardware::Hardware;
use serde_jsonlines::JsonLinesWriter;
use signal_hook::{
    consts::{SIGINT, SIGTERM},
    iterator::Signals,
};
use std::{
    env, fs,
    io::{self, BufReader},
    ops::Range,
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    sync::mpsc::{self, Receiver, SyncSender},
    thread,
    time::{Duration, Instant},
};
use whisplay_driver::{
    shared::{invalid, pixel_bytes, read_message, snapshot, Reply, Request},
    DEFAULT_SOCKET,
};

const MAX_INPUT: usize = 16 * 1024 * 1024;

enum Event {
    Request {
        command: Command,
        reply: SyncSender<io::Result<Reply>>,
    },
    Shutdown,
}

// The hardware owner receives snapshots, never client file paths.
enum Command {
    DisplayFile {
        bytes: Vec<u8>,
        columns: Range<u16>,
        rows: Range<u16>,
    },
    DisplayPixels {
        pixels: Vec<u8>,
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

fn main() {
    if let Err(error) = run() {
        eprintln!("whisplayd: {error}");
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let mut socket = PathBuf::from(DEFAULT_SOCKET);
    let mut simulate = false;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => {
                socket = args
                    .next()
                    .ok_or_else(|| invalid("--socket requires a path"))?
                    .into()
            }
            "--simulate" => simulate = true,
            "--help" | "-h" => {
                println!("whisplayd [--socket PATH] [--simulate]");
                return Ok(());
            }
            _ => return Err(invalid(format!("unknown argument: {arg}"))),
        }
    }

    // Never unlink an existing socket: it may belong to a running daemon.
    let listener = UnixListener::bind(&socket)?;
    let _socket = SocketPath(socket.to_owned());
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    let mut hardware = Hardware::open(simulate)?;
    let mut display = Display::new();
    let (send, receive) = mpsc::sync_channel(1);
    let mut signals = Signals::new([SIGINT, SIGTERM])?;
    let shutdown = send.clone();
    thread::Builder::new()
        .name("whisplay-signals".into())
        .spawn(move || {
            if signals.forever().next().is_some() {
                let _ = shutdown.send(Event::Shutdown);
            }
        })?;
    thread::Builder::new()
        .name("whisplay-socket".into())
        .spawn(move || {
            // Slow/fragmented socket reads cannot stall animation or hardware ownership.
            for stream in listener.incoming() {
                match stream {
                    Ok(mut stream) => {
                        let result = handle(&mut stream, &send);
                        let reply = match result {
                            Ok(reply) => reply,
                            Err(error) => Reply::Error {
                                message: error.to_string(),
                            },
                        };
                        let _ = JsonLinesWriter::new(&mut stream).write(&reply);
                    }
                    Err(error) => {
                        eprintln!("display socket: {error}");
                        break;
                    }
                }
            }
        })?;
    event_loop(receive, &mut display, &mut hardware)
}

fn event_loop(
    receive: Receiver<Event>,
    display: &mut Display,
    hardware: &mut Hardware,
) -> io::Result<()> {
    loop {
        let event = match display.deadline {
            Some(due) => {
                match receive.recv_timeout(due.saturating_duration_since(Instant::now())) {
                    Ok(event) => Some(event),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            None => Some(receive.recv().map_err(io::Error::other)?),
        };
        match event {
            Some(Event::Shutdown) => break,
            Some(Event::Request { command, reply }) => {
                let _ = reply.send(execute(command, display, hardware));
            }
            None => {}
        }
        // Check after every command as well as timeout: incoming requests cannot
        // starve frame updates. At most one frame is processed before checking commands.
        if let Err(error) = display.advance(Instant::now(), hardware) {
            eprintln!("display: {error}");
        }
    }
    Ok(())
}

fn execute(command: Command, display: &mut Display, hardware: &mut Hardware) -> io::Result<Reply> {
    match command {
        Command::DisplayFile {
            bytes,
            columns,
            rows,
        } => display.display_file(bytes, columns, rows)?,
        Command::DisplayPixels {
            pixels,
            columns,
            rows,
        } => display.display_pixels(pixels, columns, rows)?,
        Command::Clear => display.clear(),
        Command::Backlight { percent } => hardware.set_backlight(percent)?,
        Command::Led { rgb } => hardware.set_led(rgb)?,
        Command::State => return Ok(Reply::State(display.state())),
    }
    Ok(Reply::Ok)
}

fn handle(stream: &mut UnixStream, send: &SyncSender<Event>) -> io::Result<Reply> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let request: Request = read_message(&mut BufReader::new(&mut *stream))?;
    let command = match request {
        Request::DisplayFile {
            path,
            columns,
            rows,
        } => {
            pixel_bytes(&columns, &rows)?;
            Command::DisplayFile {
                bytes: snapshot(&path, MAX_INPUT)?,
                columns,
                rows,
            }
        }
        Request::DisplayPixels {
            path,
            columns,
            rows,
        } => {
            let expected = pixel_bytes(&columns, &rows)?;
            let pixels = snapshot(&path, expected)?;
            if pixels.len() != expected {
                return Err(invalid("incorrect shared pixel length"));
            }
            Command::DisplayPixels {
                pixels,
                columns,
                rows,
            }
        }
        Request::Clear => Command::Clear,
        Request::Backlight { percent } => Command::Backlight { percent },
        Request::Led { rgb } => Command::Led { rgb },
        Request::State => Command::State,
    };
    let (reply, receive) = mpsc::sync_channel(1);
    send.send(Event::Request { command, reply })
        .map_err(io::Error::other)?;
    receive.recv().map_err(io::Error::other)?
}

struct SocketPath(PathBuf);
impl Drop for SocketPath {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
