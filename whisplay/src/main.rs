//! Short-lived CLI over the libraries; playback belongs to whisplayd.

use clap::{Args, Parser, Subcommand};
use std::{io, ops::Range, path::PathBuf};
use whisplay::Client;
use whisplay_driver::{
    shared::{invalid, pixel_bytes, snapshot},
    DEFAULT_SOCKET,
};

/// Display messages, images and animations on Whisplay.
#[derive(Parser)]
struct Cli {
    /// Daemon socket path.
    #[arg(
        long,
        global = true,
        env = "WHISPLAY_SOCKET",
        default_value = DEFAULT_SOCKET,
        value_name = "PATH"
    )]
    socket: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Control the LCD screen.
    #[command(subcommand)]
    Display(DisplayCommand),
    /// Read display state (blank/static/playing/failed) and any playback error as JSON.
    State,
}

#[derive(Subcommand)]
enum DisplayCommand {
    /// Display a color/animated emoji from installed local assets.
    Emoji { emoji: String },
    /// Display text; aim for about 140 characters including spaces.
    ///
    /// This is an estimate for ordinary English with the default font. Wide
    /// characters, extra line breaks or a custom font may fit fewer. Overflow
    /// returns an error; text is never truncated or shrunk.
    Text { text: String },
    /// Display text and an emoji; aim for about 60 characters including spaces.
    ///
    /// This is an estimate for ordinary English with the default font. Wide
    /// characters, extra line breaks or a custom font may fit fewer. Overflow
    /// returns an error; text is never truncated or shrunk.
    TextAndEmoji { text: String, emoji: String },
    /// Display an image from a file or packed raw pixels.
    Image {
        #[command(flatten)]
        input: ImageInput,
        #[command(flatten)]
        region: Region,
    },
    /// Play a GIF animation; MP4, WebM and other video formats are unsupported.
    Video {
        /// Local GIF file; timing and loops come from the file.
        path: PathBuf,
        #[command(flatten)]
        region: Region,
    },
    /// Cancel playback and clear the screen to black.
    Clear,
    /// Set LCD screen brightness: 0=off, 100=full.
    Brightness {
        #[arg(value_parser = clap::value_parser!(u8).range(0..=100))]
        percent: u8,
    },
}

#[derive(Args)]
#[group(required = true, multiple = false)]
struct ImageInput {
    /// Local PNG, JPEG or GIF; fit inside the selected region.
    #[arg(long, value_name = "PATH")]
    file: Option<PathBuf>,
    /// Packed big-endian RGB565 file: region width × height × 2 bytes, no padding.
    #[arg(long, value_name = "PATH")]
    raw_pixels: Option<PathBuf>,
}

#[derive(Args)]
struct Region {
    /// Screen columns, end excluded; must be nonempty and inside 0..240.
    #[arg(
        long,
        default_value = "0..240",
        value_name = "START..END",
        value_parser = range
    )]
    columns: Range<u16>,
    /// Screen rows, end excluded; must be nonempty and inside 0..280.
    #[arg(
        long,
        default_value = "0..280",
        value_name = "START..END",
        value_parser = range
    )]
    rows: Range<u16>,
}

fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("whisplay: {error}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> io::Result<()> {
    let mut client = Client::new(&cli.socket);
    match cli.command {
        Command::Display(command) => match command {
            DisplayCommand::Emoji { emoji } => {
                whisplay::display_text(&mut client, "", Some(&emoji))?;
            }
            DisplayCommand::Text { text } => {
                whisplay::display_text(&mut client, &text, None)?;
            }
            DisplayCommand::TextAndEmoji { text, emoji } => {
                whisplay::display_text(&mut client, &text, Some(&emoji))?;
            }
            DisplayCommand::Image { input, region } => match (input.file, input.raw_pixels) {
                (Some(path), None) => {
                    client.display_file(&path, region.columns, region.rows)?;
                }
                (None, Some(path)) => {
                    let expected = pixel_bytes(&region.columns, &region.rows)?;
                    let pixels = snapshot(&path, expected)?;
                    client.display_pixels(&pixels, region.columns, region.rows)?;
                }
                _ => unreachable!("clap requires exactly one image input"),
            },
            DisplayCommand::Video { path, region } => {
                client.display_file(&path, region.columns, region.rows)?;
            }
            DisplayCommand::Clear => client.clear()?,
            DisplayCommand::Brightness { percent } => client.set_backlight(percent)?,
        },
        Command::State => {
            serde_json::to_writer(io::stdout().lock(), &client.state()?)?;
            println!();
            return Ok(());
        }
    }
    println!("{{\"ok\":true}}");
    Ok(())
}

fn range(value: &str) -> io::Result<Range<u16>> {
    let (start, end) = value
        .split_once("..")
        .ok_or_else(|| invalid("expected START..END (end excluded), e.g. 20..80"))?;
    Ok(start.parse().map_err(invalid)?..end.parse().map_err(invalid)?)
}
