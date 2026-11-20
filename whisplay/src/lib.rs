//! Present text and local color/animated emoji through the Whisplay daemon.
//!
//! [`display_text`] renders the static background; [`Client`] submits images,
//! animations or raw pixels directly. The daemon owns playback after the caller exits.
//!
//! # Configuration
//!
//! Assets and fonts must already exist locally; display operations never download
//! them. Missing emoji assets or font glyphs are errors.
//!
//! | Setting | Purpose | Default |
//! | --- | --- | --- |
//! | `WHISPLAY_EMOJI_DIR` | Local GIF/PNG emoji directory | `$HOME/.local/share/whisplay/emoji` |
//! | `WHISPLAY_FONT` | Text font file | `/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf` |
//!
//! Emoji filenames use lowercase Unicode code points joined by underscores,
//! omitting FE0F: `1f440.gif`, `2764.png`, `1f469_200d_1f4bb.gif`.
//! GIF takes precedence when both formats exist. No asset collection is bundled;
//! provide your own licensed files or provision a collection such as
//! [Google Noto Emoji](https://googlefonts.github.io/noto-emoji-files/?view=docs).
//!
//! # CLI
//!
//! Run `whisplay --help` or subcommand help for arguments and bounds. The CLI uses
//! `--socket PATH` or `WHISPLAY_SOCKET`, defaulting to
//! [`whisplay_driver::DEFAULT_SOCKET`]. Successful submissions print `{"ok":true}`;
//! `state` prints the serialized [`State`]. Errors go to stderr with a nonzero exit.
//! Text starting with a dash can follow `--`: `whisplay display text -- '--help'`.
//!
//! # Example
//!
//! ```no_run
//! use std::path::Path;
//! use whisplay::{display_text, Client};
//! use whisplay_driver::DEFAULT_SOCKET;
//!
//! # fn main() -> std::io::Result<()> {
//! let mut display = Client::new(Path::new(DEFAULT_SOCKET));
//! display_text(&mut display, "Looking into it", Some("👀"))?;
//! # Ok(())
//! # }
//! ```

pub use whisplay_driver::{Client, DisplayState, State};

use glyph_brush_layout::{
    ab_glyph::{Font, FontRef, ScaleFont},
    FontId, GlyphPositioner, HorizontalAlign, Layout, SectionGeometry, SectionText,
};
use std::{
    env, io,
    path::{Path, PathBuf},
};
use whisplay_driver::{
    shared::{invalid, snapshot},
    HEIGHT, WIDTH,
};

const TEXT_SIZE: f32 = 24.0;
const TEXT_WRAP_WIDTH: f32 = 216.0;
const TEXT_BOTTOM: f32 = 268.0;

/// Display a short message, optionally with a local color/animated emoji.
///
/// Aim for about 60 characters with an emoji or 140 for text alone, including
/// spaces. These are estimates for ordinary English with the default font;
/// wide characters, extra line breaks or a custom font may fit fewer. Actual
/// layout determines acceptance, not a hard character limit. Overflow errors
/// report the supplied count and suggested budget before any display submission.
/// Text is centered and word-wrapped, never truncated or shrunk.
/// It is white on black, vertically centered when displayed alone. With an emoji,
/// the emoji occupies a centered region above the text. Complex script shaping
/// is not supported. Font and asset settings are in the
/// [crate configuration](crate#configuration).
///
/// Emoji input must be one recognized Unicode emoji sequence, including any
/// skin-tone modifiers or joiners. It is normalized before local asset lookup.
///
/// Text is submitted once; the daemon decodes and plays the original emoji file.
/// These are two requests, not an atomic update. A missing asset or invalid text
/// is rejected before submission; a later file error can leave the new text.
/// No downloads, image decoding or animation encoding happen in this library.
pub fn display_text(client: &mut Client, text: &str, emoji: Option<&str>) -> io::Result<()> {
    if text.is_empty() && emoji.is_none() {
        return Err(invalid("provide text, an emoji, or both"));
    }
    let asset = emoji.map(emoji_asset).transpose()?;
    let pixels = text_pixels(text, emoji.is_some())?;
    client.display_pixels(&pixels, 0..WIDTH, 0..HEIGHT)?;
    if let Some(path) = asset {
        let top = if text.is_empty() { 76 } else { 12 };
        client.display_file(&path, 56..184, top..top + 128)?;
    }
    Ok(())
}

fn text_pixels(text: &str, with_emoji: bool) -> io::Result<Vec<u8>> {
    let mut pixels = vec![0; usize::from(WIDTH) * usize::from(HEIGHT) * 2];
    if text.is_empty() {
        return Ok(pixels);
    }
    let path = env::var_os("WHISPLAY_FONT")
        .unwrap_or_else(|| "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf".into());
    let data = snapshot(Path::new(&path), 16 * 1024 * 1024)?;
    let font = FontRef::try_from_slice(&data).map_err(invalid)?;
    if text
        .chars()
        .any(|ch| !ch.is_whitespace() && font.glyph_id(ch).0 == 0)
    {
        return Err(invalid(
            "text font lacks a requested glyph; choose WHISPLAY_FONT",
        ));
    }
    // Preserve 24 pixels per em. ab_glyph's scale denotes ascent-to-descent height.
    let units = font
        .units_per_em()
        .ok_or_else(|| invalid("font has no em size"))?;
    let scale = TEXT_SIZE * (font.ascent_unscaled() - font.descent_unscaled()) / units;
    let glyphs = Layout::default()
        .h_align(HorizontalAlign::Center)
        .calculate_glyphs(
            std::slice::from_ref(&font),
            &SectionGeometry {
                screen_position: (120.0, 0.0),
                bounds: (TEXT_WRAP_WIDTH, f32::INFINITY),
            },
            &[SectionText {
                text,
                scale: scale.into(),
                font_id: FontId(0),
            }],
        );
    let descent = font.as_scaled(scale).descent();
    let height = glyphs
        .iter()
        .map(|glyph| glyph.glyph.position.y - descent)
        .fold(0.0, f32::max);
    let top = if with_emoji { 152.0 } else { 12.0 };
    let available_height = TEXT_BOTTOM - top;
    let overflow = || {
        let (budget, layout) = if with_emoji {
            (60, "with an emoji")
        } else {
            (140, "without an emoji")
        };
        invalid(format!(
            "text does not fit: received {} characters; aim for about {budget} \
             characters {layout}, including spaces. Wide characters, extra line \
             breaks or a custom font may fit fewer; shorten the message or remove line breaks",
            text.chars().count(),
        ))
    };
    if height > available_height {
        return Err(overflow());
    }
    let shift = top + (available_height - height) / 2.0;
    for positioned in glyphs {
        let mut glyph = positioned.glyph;
        glyph.position.y += shift;
        if let Some(outline) = font.outline_glyph(glyph) {
            let bounds = outline.px_bounds();
            if bounds.min.x < 0.0
                || bounds.max.x > f32::from(WIDTH)
                || bounds.min.y < top
                || bounds.max.y > TEXT_BOTTOM
            {
                return Err(overflow());
            }
            outline.draw(|x, y, coverage| {
                let level = (coverage * 255.0).round() as u16;
                let color = ((level >> 3) << 11) | ((level >> 2) << 5) | (level >> 3);
                let offset = ((bounds.min.y as usize + y as usize) * usize::from(WIDTH)
                    + bounds.min.x as usize
                    + x as usize)
                    * 2;
                // Kerning can overlap glyph bounds; keep neighboring white ink.
                let previous = u16::from_be_bytes([pixels[offset], pixels[offset + 1]]);
                pixels[offset..offset + 2].copy_from_slice(&previous.max(color).to_be_bytes());
            });
        }
    }
    Ok(pixels)
}

fn emoji_asset(emoji: &str) -> io::Result<PathBuf> {
    let key = emoji_key(emoji)?;
    let directory = env::var_os("WHISPLAY_EMOJI_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share/whisplay/emoji"))
        })
        .ok_or_else(|| invalid("set WHISPLAY_EMOJI_DIR"))?;
    for extension in ["gif", "png"] {
        let path = directory.join(format!("{key}.{extension}"));
        if path.is_file() {
            return Ok(path);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "local emoji {key}.gif or {key}.png not found in {}",
            directory.display()
        ),
    ))
}

fn emoji_key(emoji: &str) -> io::Result<String> {
    let emoji = emojis::get(emoji)
        .ok_or_else(|| invalid("expected a single supported Unicode emoji, e.g. 👀 or 👩‍💻"))?;
    // Keep the installed asset convention after normalizing the Unicode sequence.
    Ok(emoji
        .as_str()
        .chars()
        .filter(|ch| *ch != '\u{fe0f}')
        .map(|ch| format!("{:x}", u32::from(ch)))
        .collect::<Vec<_>>()
        .join("_"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emoji_keys_normalize_valid_sequences_and_preserve_asset_filenames() {
        assert_eq!(emoji_key("❤️").unwrap(), "2764");
        assert_eq!(emoji_key("❤").unwrap(), "2764");
        assert_eq!(emoji_key("👍🏽").unwrap(), "1f44d_1f3fd");
        assert_eq!(emoji_key("👩‍💻").unwrap(), "1f469_200d_1f4bb");
        assert!(emoji_key("").is_err());
        assert!(emoji_key("hello").is_err());
        assert!(emoji_key("👀👀").is_err());
    }
}
