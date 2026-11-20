//! One display: static updates, GIF playback, frame deadlines and replacement.

use super::hardware::Hardware;
use fast_image_resize::{
    images::{TypedImage, TypedImageRef},
    pixels::U8x4,
    FilterType, ResizeAlg, ResizeOptions, Resizer,
};
use gif_dispose::{ImgRef, Screen};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits, RgbaImage};
use std::{
    io::{self, Cursor},
    ops::Range,
    rc::Rc,
    time::{Duration, Instant},
};
use whisplay_driver::{
    shared::{invalid, pixel_bytes},
    DisplayState, State, HEIGHT, WIDTH,
};

const MAX_PIXELS: u64 = 4_000_000;
const MAX_CACHE_BYTES: usize = 8 * 1024 * 1024;

pub(super) struct Display {
    animation: Option<GifAnimation>,
    pending: Option<Pending>,
    pub(super) deadline: Option<Instant>,
    blank: bool,
    error: Option<String>,
}

enum Pending {
    Frame(Update),
    Clear,
}

struct Update {
    pixels: Vec<u8>,
    columns: Range<u16>,
    rows: Range<u16>,
    delay: Duration,
}

struct GifAnimation {
    decoder: Option<GifDecoder>,
    cache: Option<FrameCache>,
    pixels: Vec<u8>,
    frame_count: usize,
    frames_left: usize,
    passes: Option<u32>, // None means the file requests infinite looping.
    columns: Range<u16>,
    rows: Range<u16>,
}

/// Streaming decode, composition and resize storage, released once caching finishes.
struct GifDecoder {
    data: Rc<[u8]>,
    reader: gif::Decoder<Cursor<Rc<[u8]>>>,
    screen: Screen,
    indices: Vec<u8>,
    renderer: FrameRenderer,
}

/// Final pixels and delays for one animation, bounded by MAX_CACHE_BYTES.
struct FrameCache {
    pixels: Vec<u8>,
    delays: Vec<Duration>,
    frame_bytes: usize,
}

impl Display {
    pub(super) fn new() -> Self {
        Self {
            animation: None,
            pending: None,
            deadline: None,
            blank: true,
            error: None,
        }
    }

    pub(super) fn state(&self) -> State {
        State {
            state: if self.error.is_some() {
                DisplayState::Failed
            } else if self.animation.is_some() {
                DisplayState::Playing
            } else if self.blank {
                DisplayState::Blank
            } else {
                DisplayState::Static
            },
            error: self.error.clone(),
        }
    }

    /// Prepare the first frame before replacing a working presentation.
    pub(super) fn display_file(
        &mut self,
        bytes: Vec<u8>,
        columns: Range<u16>,
        rows: Range<u16>,
    ) -> io::Result<()> {
        pixel_bytes(&columns, &rows)?;
        let area_width = columns.end - columns.start;
        let area_height = rows.end - rows.start;
        let format = image::guess_format(&bytes).map_err(invalid)?;
        let (pixels, delay, animation) = if format == ImageFormat::Gif {
            let mut animation = GifAnimation::new(bytes.into(), columns.clone(), rows.clone())?;
            let first = animation
                .next()?
                .ok_or_else(|| invalid("GIF has no frames"))?;
            (first.pixels, first.delay, Some(animation))
        } else {
            if !matches!(format, ImageFormat::Png | ImageFormat::Jpeg) {
                return Err(invalid("supported media: PNG, JPEG, GIF"));
            }
            let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
            reader.limits(limits());
            let decoder = reader.into_decoder().map_err(invalid)?;
            let (width, height) = decoder.dimensions();
            dimensions(width, height)?;
            let decoded = DynamicImage::from_decoder(decoder)
                .map_err(invalid)?
                .into_rgba8();
            (
                fit(&decoded, area_width, area_height)?,
                Duration::ZERO,
                None,
            )
        };
        self.animation = animation;
        if let Some(animation) = &mut self.animation {
            // Drop the previous animation before reserving another cache.
            animation.start_cache(&pixels, delay);
        }
        self.blank = false;
        self.error = None;
        self.pending = Some(Pending::Frame(Update {
            pixels,
            columns,
            rows,
            delay,
        }));
        self.deadline = Some(Instant::now());
        Ok(())
    }

    pub(super) fn display_pixels(
        &mut self,
        pixels: Vec<u8>,
        columns: Range<u16>,
        rows: Range<u16>,
    ) -> io::Result<()> {
        if pixel_bytes(&columns, &rows)? != pixels.len() {
            return Err(invalid("incorrect pixel length"));
        }
        self.animation = None;
        self.blank = false;
        self.error = None;
        self.pending = Some(Pending::Frame(Update {
            pixels,
            columns,
            rows,
            delay: Duration::ZERO,
        }));
        self.deadline = Some(Instant::now());
        Ok(())
    }

    pub(super) fn clear(&mut self) {
        self.animation = None;
        self.blank = true;
        self.error = None;
        self.pending = Some(Pending::Clear);
        self.deadline = Some(Instant::now());
    }

    /// Process at most one animation frame per turn so commands stay responsive.
    pub(super) fn advance(&mut self, now: Instant, hardware: &mut Hardware) -> io::Result<()> {
        let Some(due) = self.deadline.filter(|due| *due <= now) else {
            return Ok(());
        };
        let result = self.advance_frame(now, due, hardware);
        if let Err(error) = &result {
            self.animation = None;
            self.pending = None;
            self.deadline = None;
            self.error = Some(error.to_string());
        }
        result
    }

    fn advance_frame(
        &mut self,
        now: Instant,
        due: Instant,
        hardware: &mut Hardware,
    ) -> io::Result<()> {
        let first = self.pending.is_some();
        let Update {
            pixels,
            columns,
            rows,
            delay,
        } = if let Some(pending) = self.pending.take() {
            match pending {
                Pending::Frame(update) => update,
                Pending::Clear => {
                    let black = vec![0; usize::from(WIDTH) * usize::from(HEIGHT) * 2];
                    hardware.write_pixels(&black, 0..WIDTH, 0..HEIGHT)?;
                    self.deadline = None;
                    return Ok(());
                }
            }
        } else if let Some(animation) = &mut self.animation {
            match animation.next()? {
                Some(update) => update,
                None => {
                    self.animation = None;
                    self.deadline = None;
                    return Ok(());
                }
            }
        } else {
            self.deadline = None;
            return Ok(());
        };
        // If decoding itself cannot keep up, rebase after two missed periods.
        // Accumulating unlimited timing debt would otherwise freeze LCD output.
        let next_due = if now.saturating_duration_since(due) > delay.saturating_mul(2) {
            now + delay
        } else {
            due + delay
        };
        // The decoder still composes every frame. Only skip an obsolete LCD write.
        // Always write the first frame and the final frame of a finite animation.
        let final_frame = self
            .animation
            .as_ref()
            .is_some_and(|animation| animation.passes == Some(1) && animation.frames_left == 0);
        if first || final_frame || self.animation.is_none() || next_due > now {
            hardware.write_pixels(&pixels, columns, rows)?;
        }
        if let Some(animation) = &mut self.animation {
            animation.pixels = pixels;
        }
        self.deadline = self.animation.as_ref().map(|_| next_due);
        Ok(())
    }
}

impl GifAnimation {
    fn new(data: Rc<[u8]>, columns: Range<u16>, rows: Range<u16>) -> io::Result<Self> {
        let mut options = gif::DecodeOptions::new();
        options.skip_frame_decoding(true);
        let mut probe = options.read_info(Cursor::new(&*data)).map_err(invalid)?;
        let (width, height) = (probe.width(), probe.height());
        dimensions(u32::from(width), u32::from(height))?;
        let canvas_bytes = usize::from(width) * usize::from(height) * 4;
        let mut largest_frame = canvas_bytes / 4;
        let mut frame_count = 0;
        // Preserve initial structural validation, and count frames without LZW decoding.
        // gif 0.13 only reports loop extensions encountered before the first frame.
        while let Some(frame) = probe.read_next_frame().map_err(invalid)? {
            let pixels = usize::from(frame.width) * usize::from(frame.height);
            let partial =
                frame.left != 0 || frame.top != 0 || frame.width != width || frame.height != height;
            if canvas_bytes + pixels * 4 + usize::from(partial) * canvas_bytes > 64 * 1024 * 1024 {
                return Err(invalid("GIF frame exceeds decoder memory limit"));
            }
            largest_frame = largest_frame.max(pixels);
            frame_count += 1;
        }
        if frame_count == 0 {
            return Err(invalid("GIF has no frames"));
        }
        let passes = match probe.repeat() {
            gif::Repeat::Infinite => None,
            gif::Repeat::Finite(repeats) => Some(u32::from(repeats) + 1),
        };
        let reader = gif_reader(Rc::clone(&data))?;
        Ok(Self {
            decoder: Some(GifDecoder {
                screen: Screen::new_decoder(&reader),
                renderer: FrameRenderer::new(
                    u32::from(width),
                    u32::from(height),
                    columns.end - columns.start,
                    rows.end - rows.start,
                )?,
                reader,
                data,
                indices: vec![0; largest_frame],
            }),
            cache: None,
            pixels: Vec::new(),
            frame_count,
            frames_left: frame_count,
            passes,
            columns,
            rows,
        })
    }

    fn start_cache(&mut self, pixels: &[u8], delay: Duration) {
        if self.passes != Some(1) {
            self.cache = FrameCache::new(self.frame_count, pixels, delay);
            if self.cache.is_some() && self.frames_left == 0 {
                self.decoder = None;
            }
        }
    }

    fn next(&mut self) -> io::Result<Option<Update>> {
        if self.frames_left == 0 {
            match self.passes {
                Some(1) => return Ok(None),
                Some(ref mut passes) => *passes -= 1,
                None => {}
            }
            if let Some(decoder) = &mut self.decoder {
                decoder.restart()?;
            }
            self.frames_left = self.frame_count;
        }
        let delay = if let Some(decoder) = &mut self.decoder {
            let delay = decoder.next(&mut self.pixels)?;
            if let Some(cache) = &mut self.cache {
                cache.pixels.extend_from_slice(&self.pixels);
                cache.delays.push(delay);
                if self.frames_left == 1 {
                    self.decoder = None;
                }
            }
            delay
        } else {
            let cache = self.cache.as_ref().expect("decoded frames are cached");
            let index = self.frame_count - self.frames_left;
            let start = index * cache.frame_bytes;
            self.pixels.clear();
            self.pixels
                .extend_from_slice(&cache.pixels[start..start + cache.frame_bytes]);
            cache.delays[index]
        };
        self.frames_left -= 1;
        Ok(Some(Update {
            pixels: std::mem::take(&mut self.pixels),
            columns: self.columns.clone(),
            rows: self.rows.clone(),
            delay,
        }))
    }
}

impl FrameCache {
    fn new(frames: usize, pixels: &[u8], delay: Duration) -> Option<Self> {
        let bytes = pixels
            .len()
            .checked_add(std::mem::size_of::<Duration>())?
            .checked_mul(frames)?;
        if bytes > MAX_CACHE_BYTES {
            return None;
        }
        let mut cache = Self {
            pixels: Vec::new(),
            delays: Vec::new(),
            frame_bytes: pixels.len(),
        };
        // A cache is optional: allocation failure keeps the streaming path.
        cache.pixels.try_reserve_exact(pixels.len() * frames).ok()?;
        cache.delays.try_reserve_exact(frames).ok()?;
        cache.pixels.extend_from_slice(pixels);
        cache.delays.push(delay);
        Some(cache)
    }
}

impl GifDecoder {
    fn restart(&mut self) -> io::Result<()> {
        self.reader = gif_reader(Rc::clone(&self.data))?;
        // The compositor has no reset API. A transparent full-canvas frame
        // schedules background disposal before the next real frame, reusing storage.
        self.indices.fill(0);
        self.screen
            .blit(
                Some(&[gif_dispose::RGB8::default()]),
                gif::DisposalMethod::Background,
                0,
                0,
                ImgRef::new(
                    &self.indices[..self.screen.width() * self.screen.height()],
                    self.screen.width(),
                    self.screen.height(),
                ),
                Some(0),
            )
            .map_err(invalid)
    }

    fn next(&mut self, pixels: &mut Vec<u8>) -> io::Result<Duration> {
        let frame = self
            .reader
            .next_frame_info()
            .map_err(invalid)?
            .ok_or_else(|| invalid("GIF ended early"))?;
        let (left, top, width, height, disposal, transparent, delay) = (
            frame.left,
            frame.top,
            frame.width,
            frame.height,
            frame.dispose,
            frame.transparent,
            frame.delay,
        );
        let length = usize::from(width) * usize::from(height);
        self.reader
            .read_into_buffer(&mut self.indices[..length])
            .map_err(invalid)?;
        let palette = bytemuck::cast_slice(self.reader.palette().map_err(invalid)?);
        // Match image's clipping of frames that extend beyond the logical canvas.
        let clipped_width =
            usize::from(width).min(self.screen.width().saturating_sub(usize::from(left)));
        let clipped_height =
            usize::from(height).min(self.screen.height().saturating_sub(usize::from(top)));
        if clipped_width != 0 && clipped_height != 0 {
            let indices = ImgRef::new(
                &self.indices[..length],
                usize::from(width),
                usize::from(height),
            );
            self.screen
                .blit(
                    Some(palette),
                    disposal,
                    left,
                    top,
                    indices.sub_image(0, 0, clipped_width, clipped_height),
                    transparent,
                )
                .map_err(invalid)?;
        } else {
            // An off-canvas frame still disposes its predecessor and consumes its delay.
            self.screen
                .blit(
                    Some(palette),
                    gif::DisposalMethod::Keep,
                    0,
                    0,
                    ImgRef::new(&[0], 1, 1),
                    Some(0),
                )
                .map_err(invalid)?;
        }
        let canvas = self.screen.pixels_rgba();
        self.renderer
            .render(bytemuck::cast_slice(canvas.buf()), pixels)?;
        Ok(Duration::from_millis(u64::from(delay) * 10).max(Duration::from_millis(1)))
    }
}

fn gif_reader(data: Rc<[u8]>) -> io::Result<gif::Decoder<Cursor<Rc<[u8]>>>> {
    let mut options = gif::DecodeOptions::new();
    options.set_memory_limit(gif::MemoryLimit::Bytes(
        (64 * 1024 * 1024).try_into().unwrap(),
    ));
    options.read_info(Cursor::new(data)).map_err(invalid)
}

fn limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_PIXELS as u32);
    limits.max_image_height = Some(MAX_PIXELS as u32);
    limits.max_alloc = Some(64 * 1024 * 1024);
    limits
}

fn dimensions(width: u32, height: u32) -> io::Result<()> {
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(invalid("media canvas must contain 1..=4,000,000 pixels"));
    }
    Ok(())
}

fn fit(source: &RgbaImage, area_width: u16, area_height: u16) -> io::Result<Vec<u8>> {
    dimensions(source.width(), source.height())?;
    let mut renderer =
        FrameRenderer::new(source.width(), source.height(), area_width, area_height)?;
    let mut pixels = Vec::new();
    renderer.render(source.as_raw(), &mut pixels)?;
    Ok(pixels)
}

/// Reuse resized pixels, resizer scratch space and the caller's RGB565 output.
struct FrameRenderer {
    resizer: Resizer,
    source_width: u32,
    source_height: u32,
    resized: RgbaImage,
    area_width: u16,
    area_height: u16,
}

impl FrameRenderer {
    fn new(
        source_width: u32,
        source_height: u32,
        area_width: u16,
        area_height: u16,
    ) -> io::Result<Self> {
        let (width, height) = if u64::from(source_width) * u64::from(area_height)
            > u64::from(source_height) * u64::from(area_width)
        {
            (
                u32::from(area_width),
                (source_height * u32::from(area_width) / source_width).max(1),
            )
        } else {
            (
                (source_width * u32::from(area_height) / source_height).max(1),
                u32::from(area_height),
            )
        };
        Ok(Self {
            resizer: Resizer::new(),
            source_width,
            source_height,
            resized: RgbaImage::new(width, height),
            area_width,
            area_height,
        })
    }

    fn render(&mut self, rgba: &[u8], pixels: &mut Vec<u8>) -> io::Result<()> {
        let source =
            TypedImageRef::<U8x4>::from_buffer(self.source_width, self.source_height, rgba)
                .map_err(invalid)?;
        let mut destination = TypedImage::<U8x4>::from_buffer(
            self.resized.width(),
            self.resized.height(),
            self.resized.as_mut(),
        )
        .map_err(invalid)?;
        // SIMD bilinear filtering borrows our image buffers and reuses resizer scratch.
        // Filter straight RGBA, then composite over black below.
        let options = ResizeOptions::new()
            .resize_alg(ResizeAlg::Convolution(FilterType::Bilinear))
            .use_alpha(false);
        self.resizer
            .resize_typed(&source, &mut destination, &options)
            .map_err(invalid)?;
        pixels.resize(
            usize::from(self.area_width) * usize::from(self.area_height) * 2,
            0,
        );
        pixels.fill(0);
        let left = (u32::from(self.area_width) - self.resized.width()) / 2;
        let top = (u32::from(self.area_height) - self.resized.height()) / 2;
        for (x, y, rgba) in self.resized.enumerate_pixels() {
            let [r, g, b, a] = rgba.0.map(u16::from);
            let rgb =
                (((r * a / 255) >> 3) << 11) | (((g * a / 255) >> 2) << 5) | ((b * a / 255) >> 3);
            let offset = (((y + top) * u32::from(self.area_width) + x + left) * 2) as usize;
            pixels[offset..offset + 2].copy_from_slice(&rgb.to_be_bytes());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(hardware: &Hardware) -> &[u8] {
        match hardware {
            Hardware::Simulated(pixels) => pixels,
            Hardware::Physical(_) => panic!("expected simulated hardware"),
        }
    }

    fn gif(repeat: gif::Repeat) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = gif::Encoder::new(&mut bytes, 1, 1, &[]).unwrap();
            encoder.set_repeat(repeat).unwrap();
            for rgba in [[255, 0, 0, 255], [0, 255, 0, 255]] {
                let mut data = rgba;
                let mut frame = gif::Frame::from_rgba_speed(1, 1, &mut data, 30);
                frame.delay = 10;
                encoder.write_frame(&frame).unwrap();
            }
        }
        bytes
    }

    #[test]
    fn fit_centers_media_and_composites_transparency_on_black() {
        let source = RgbaImage::from_pixel(1, 1, image::Rgba([255, 255, 255, 128]));
        let pixels = fit(&source, WIDTH, HEIGHT).unwrap();
        assert_eq!(&pixels[..2], &[0, 0]);
        let center = (140 * usize::from(WIDTH) + 120) * 2;
        assert_eq!(&pixels[center..center + 2], &[0x84, 0x10]);
        assert_eq!(&pixels[pixels.len() - 2..], &[0, 0]);
    }

    #[test]
    fn streamed_gif_pixel_buffers_are_reused_across_frames_and_loops() {
        let mut animation =
            GifAnimation::new(gif(gif::Repeat::Infinite).into(), 56..184, 12..140).unwrap();
        animation.pixels = animation.next().unwrap().unwrap().pixels;
        let buffers = |animation: &mut GifAnimation| {
            let decoder = animation.decoder.as_mut().unwrap();
            let canvas = decoder.screen.pixels_rgba();
            (
                decoder.indices.as_ptr(),
                canvas.buf().as_ptr(),
                decoder.renderer.resized.as_raw().as_ptr(),
                animation.pixels.as_ptr(),
            )
        };
        let before = buffers(&mut animation);
        for _ in 0..10 {
            animation.pixels = animation.next().unwrap().unwrap().pixels;
            assert_eq!(buffers(&mut animation), before);
        }
    }

    #[test]
    fn resized_rgba_stays_within_one_quantization_step_of_image() {
        let source = RgbaImage::from_fn(16, 12, |x, y| {
            image::Rgba([
                (x * 15) as u8,
                (y * 19) as u8,
                ((x + y) * 8) as u8,
                ((x * y) % 256) as u8,
            ])
        });
        let reference =
            image::imageops::resize(&source, 8, 6, image::imageops::FilterType::Triangle);
        let actual = fit(&source, 8, 6).unwrap();
        for (actual, rgba) in actual.chunks_exact(2).zip(reference.pixels()) {
            let [r, g, b, a] = rgba.0.map(u16::from);
            let actual = u16::from_be_bytes(actual.try_into().unwrap());
            for (value, expected) in [
                ((actual >> 11) & 31, (r * a / 255) >> 3),
                ((actual >> 5) & 63, (g * a / 255) >> 2),
                (actual & 31, (b * a / 255) >> 3),
            ] {
                assert!(value.abs_diff(expected) <= 1);
            }
        }
    }

    #[test]
    fn composition_matches_image_for_disposal_transparency_clipping_and_loop_reset() {
        use image::AnimationDecoder;
        use std::borrow::Cow;
        let mut bytes = Vec::new();
        {
            let palette = [0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255];
            let mut encoder = gif::Encoder::new(&mut bytes, 3, 2, &palette).unwrap();
            encoder.set_repeat(gif::Repeat::Infinite).unwrap();
            for (left, top, width, height, dispose, pixels) in [
                (
                    0,
                    0,
                    3,
                    2,
                    gif::DisposalMethod::Keep,
                    vec![0, 1, 1, 1, 1, 1],
                ),
                (1, 0, 1, 1, gif::DisposalMethod::Previous, vec![2]),
                (0, 1, 2, 1, gif::DisposalMethod::Background, vec![0, 3]),
                (2, 1, 2, 1, gif::DisposalMethod::Previous, vec![2, 3]),
                (4, 0, 1, 1, gif::DisposalMethod::Keep, vec![2]),
                (0, 0, 1, 1, gif::DisposalMethod::Keep, vec![3]),
            ] {
                encoder
                    .write_frame(&gif::Frame {
                        left,
                        top,
                        width,
                        height,
                        dispose,
                        transparent: Some(0),
                        buffer: Cow::Owned(pixels),
                        ..Default::default()
                    })
                    .unwrap();
            }
        }
        let expected = image::codecs::gif::GifDecoder::new(Cursor::new(&bytes))
            .unwrap()
            .into_frames()
            .collect_frames()
            .unwrap();
        let mut animation = GifAnimation::new(bytes.into(), 0..3, 0..2).unwrap();
        for _ in 0..3 {
            for frame in &expected {
                let update = animation.next().unwrap().unwrap();
                let canvas = animation.decoder.as_mut().unwrap().screen.pixels_rgba();
                assert_eq!(
                    bytemuck::cast_slice::<_, u8>(canvas.buf()),
                    frame.buffer().as_raw()
                );
                animation.pixels = update.pixels;
            }
        }
    }

    #[test]
    fn partial_pixels_replace_animation_and_preserve_the_other_visible_pixels() {
        let mut hardware = Hardware::open(true).unwrap();
        let mut display = Display::new();
        display
            .display_file(gif(gif::Repeat::Infinite), 0..WIDTH, 0..HEIGHT)
            .unwrap();
        display
            .advance(display.deadline.unwrap(), &mut hardware)
            .unwrap();
        let before = screen(&hardware).to_vec();
        display
            .display_pixels(vec![0, 0x1f], 239..240, 279..280)
            .unwrap();
        display
            .advance(display.deadline.unwrap(), &mut hardware)
            .unwrap();
        display
            .advance(Instant::now() + Duration::from_secs(10), &mut hardware)
            .unwrap();
        let after = screen(&hardware);
        assert_eq!(&after[..after.len() - 2], &before[..before.len() - 2]);
        assert_eq!(&after[after.len() - 2..], &[0, 0x1f]);
        display.clear();
        display
            .advance(display.deadline.unwrap(), &mut hardware)
            .unwrap();
        assert!(screen(&hardware).iter().all(|byte| *byte == 0));
    }

    #[test]
    fn clear_cancels_pending_playback_and_queues_a_black_frame() {
        let mut hardware = Hardware::open(true).unwrap();
        let mut display = Display::new();
        assert_eq!(display.state().state, DisplayState::Blank);
        display.display_pixels(vec![0xf8, 0], 0..1, 0..1).unwrap();
        display
            .advance(display.deadline.unwrap(), &mut hardware)
            .unwrap();
        let before = screen(&hardware).to_vec();
        assert_eq!(display.state().state, DisplayState::Static);
        display
            .display_file(gif(gif::Repeat::Infinite), 0..WIDTH, 0..HEIGHT)
            .unwrap();
        assert_eq!(display.state().state, DisplayState::Playing);

        display.clear();
        assert_eq!(display.state().state, DisplayState::Blank);
        assert!(display.animation.is_none());
        assert!(matches!(display.pending, Some(Pending::Clear)));
        assert_eq!(screen(&hardware), before.as_slice());

        display
            .advance(display.deadline.unwrap(), &mut hardware)
            .unwrap();
        assert!(display.pending.is_none());
        assert!(display.deadline.is_none());
        assert!(screen(&hardware).iter().all(|byte| *byte == 0));
        display
            .advance(Instant::now() + Duration::from_secs(10), &mut hardware)
            .unwrap();
        assert!(screen(&hardware).iter().all(|byte| *byte == 0));
    }

    #[test]
    fn animation_updates_only_its_region_and_retains_the_surrounding_pixels() {
        let mut hardware = Hardware::open(true).unwrap();
        let mut display = Display::new();
        let background = [0, 0x1f].repeat(usize::from(WIDTH) * usize::from(HEIGHT));
        display
            .display_pixels(background.clone(), 0..WIDTH, 0..HEIGHT)
            .unwrap();
        display
            .advance(display.deadline.unwrap(), &mut hardware)
            .unwrap();
        display
            .display_file(gif(gif::Repeat::Infinite), 56..184, 12..140)
            .unwrap();
        for expected in [[0xf8, 0], [0x07, 0xe0]] {
            display
                .advance(display.deadline.unwrap(), &mut hardware)
                .unwrap();
            let screen = screen(&hardware);
            let center = (76 * usize::from(WIDTH) + 120) * 2;
            assert_eq!(&screen[center..center + 2], &expected);
            for row in 0..usize::from(HEIGHT) {
                let start = row * usize::from(WIDTH) * 2;
                if (12..140).contains(&row) {
                    assert_eq!(
                        &screen[start..start + 56 * 2],
                        &background[start..start + 56 * 2]
                    );
                    assert_eq!(
                        &screen[start + 184 * 2..start + 240 * 2],
                        &background[start + 184 * 2..start + 240 * 2]
                    );
                } else {
                    assert_eq!(
                        &screen[start..start + 240 * 2],
                        &background[start..start + 240 * 2]
                    );
                }
            }
        }
        assert!(display
            .display_file(gif(gif::Repeat::Infinite), 0..241, 0..1)
            .is_err());
        assert!(display.animation.is_some());
    }

    #[test]
    fn finite_gif_honors_repeats_and_leaves_its_last_frame() {
        let mut hardware = Hardware::open(true).unwrap();
        let mut display = Display::new();
        display
            .display_file(gif(gif::Repeat::Finite(1)), 0..WIDTH, 0..HEIGHT)
            .unwrap();
        let center = (140 * usize::from(WIDTH) + 120) * 2;
        for expected in [[0xf8, 0], [0x07, 0xe0], [0xf8, 0], [0x07, 0xe0]] {
            display
                .advance(display.deadline.unwrap(), &mut hardware)
                .unwrap();
            assert_eq!(&screen(&hardware)[center..center + 2], &expected);
        }
        display
            .advance(display.deadline.unwrap(), &mut hardware)
            .unwrap();
        assert!(display.animation.is_none());
        assert!(display.deadline.is_none());
        assert_eq!(display.state().state, DisplayState::Static);
    }

    #[test]
    fn overloaded_animation_rebases_its_deadline_and_keeps_updating() {
        let mut hardware = Hardware::open(true).unwrap();
        let mut display = Display::new();
        display
            .display_file(gif(gif::Repeat::Infinite), 0..WIDTH, 0..HEIGHT)
            .unwrap();
        display
            .advance(display.deadline.unwrap(), &mut hardware)
            .unwrap();
        let late = display.deadline.unwrap() + Duration::from_secs(1);
        display.advance(late, &mut hardware).unwrap();
        let center = (140 * usize::from(WIDTH) + 120) * 2;
        assert_eq!(&screen(&hardware)[center..center + 2], &[0x07, 0xe0]);
        assert!(display.deadline.unwrap() > late);
    }

    #[test]
    fn later_gif_decode_failure_keeps_the_last_written_frame() {
        let mut bytes = gif(gif::Repeat::Finite(0));
        let second = bytes
            .iter()
            .enumerate()
            .filter(|(_, byte)| **byte == 0x2c)
            .nth(1)
            .unwrap()
            .0;
        let packed = bytes[second + 9];
        let table = if packed & 0x80 != 0 {
            3 * (1 << ((packed & 7) + 1))
        } else {
            0
        };
        let block = second + 10 + table + 1;
        let length = usize::from(bytes[block]);
        bytes[block + 1..block + 1 + length].fill(0xff);
        let mut hardware = Hardware::open(true).unwrap();
        let mut display = Display::new();
        display.display_file(bytes, 0..WIDTH, 0..HEIGHT).unwrap();
        display
            .advance(display.deadline.unwrap(), &mut hardware)
            .unwrap();
        let before = screen(&hardware).to_vec();
        assert!(display
            .advance(display.deadline.unwrap(), &mut hardware)
            .is_err());
        assert!(display.animation.is_none());
        assert_eq!(screen(&hardware), before.as_slice());
        assert!(display.deadline.is_none());
        let state = display.state();
        assert_eq!(state.state, DisplayState::Failed);
        assert!(state.error.is_some());
        assert!(display.display_pixels(Vec::new(), 0..1, 0..1).is_err());
        assert_eq!(display.state().error, state.error);
        assert_eq!(display.state().state, DisplayState::Failed);
        display.clear();
        assert_eq!(display.state().state, DisplayState::Blank);
        assert!(display.state().error.is_none());
    }

    #[test]
    fn rejected_media_does_not_replace_a_working_display() {
        let mut hardware = Hardware::open(true).unwrap();
        let mut display = Display::new();
        display.display_pixels(vec![0xf8, 0], 0..1, 0..1).unwrap();
        display
            .advance(display.deadline.unwrap(), &mut hardware)
            .unwrap();
        assert!(display
            .display_file(b"not an image".to_vec(), 0..WIDTH, 0..HEIGHT)
            .is_err());
        assert_eq!(&screen(&hardware)[..2], &[0xf8, 0]);
    }
}
