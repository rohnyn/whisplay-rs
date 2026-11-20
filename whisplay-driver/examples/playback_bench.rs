//! Decode/render/write benchmark without waiting for GIF frame delays.
//! Run with --release --features daemon --example playback_bench -- FILE.gif [--full-screen] [--hardware].

#[allow(dead_code)]
#[path = "../src/bin/whisplayd/display.rs"]
mod display;
#[allow(dead_code)]
#[path = "../src/bin/whisplayd/hardware.rs"]
mod hardware;

use std::{
    alloc::{GlobalAlloc, Layout, System},
    io::{self, Cursor},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
    time::Instant,
};

struct Allocations;
static COUNTING: AtomicBool = AtomicBool::new(false);
static CALLS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
static LARGE: AtomicUsize = AtomicUsize::new(0);

fn count(bytes: usize) {
    if COUNTING.load(Relaxed) {
        CALLS.fetch_add(1, Relaxed);
        BYTES.fetch_add(bytes, Relaxed);
        LARGE.fetch_add(usize::from(bytes >= 1024 * 1024), Relaxed);
    }
}

// Instrument only this benchmark's allocator; forward every operation unchanged.
unsafe impl GlobalAlloc for Allocations {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        count(size);
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: Allocations = Allocations;

fn main() -> io::Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("provide a GIF path");
    let mut full_screen = false;
    let mut real_hardware = false;
    for arg in args {
        match arg.as_str() {
            "--full-screen" => full_screen = true,
            "--hardware" => real_hardware = true,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown argument {arg}; expected --full-screen or --hardware"),
                ))
            }
        }
    }
    let bytes = std::fs::read(path)?;
    let mut options = gif::DecodeOptions::new();
    options.skip_frame_decoding(true);
    let mut probe = options
        .read_info(Cursor::new(&bytes))
        .map_err(io::Error::other)?;
    let mut frames = 0;
    while probe.read_next_frame().map_err(io::Error::other)?.is_some() {
        frames += 1;
    }
    let mut hardware = hardware::Hardware::open(!real_hardware)?;
    let mut display = display::Display::new();
    let start = Instant::now();
    display.display_file(
        bytes,
        if full_screen {
            0..whisplay_driver::WIDTH
        } else {
            56..184
        },
        if full_screen {
            0..whisplay_driver::HEIGHT
        } else {
            12..140
        },
    )?;
    let prepare_ms = start.elapsed().as_secs_f64() * 1000.0;
    let advance = |display: &mut display::Display, hardware: &mut hardware::Hardware| {
        display.advance(
            display.deadline.expect("benchmark requires a looping GIF"),
            hardware,
        )
    };
    for _ in 0..frames * 2 {
        advance(&mut display, &mut hardware)?;
    }
    let measured_frames = frames * 5;
    COUNTING.store(true, Relaxed);
    let start = Instant::now();
    for _ in 0..measured_frames {
        advance(&mut display, &mut hardware)?;
    }
    let elapsed = start.elapsed();
    COUNTING.store(false, Relaxed);
    println!(
        "{}",
        serde_json::json!({
            "frames": measured_frames,
            "hardware": if real_hardware { "physical" } else { "simulated" },
            "display_region": if full_screen { [whisplay_driver::WIDTH, whisplay_driver::HEIGHT] } else { [128, 128] },
            "prepare_ms": prepare_ms,
            "milliseconds_per_frame": elapsed.as_secs_f64() * 1000.0 / measured_frames as f64,
            "allocation_calls": CALLS.load(Relaxed),
            "allocated_bytes": BYTES.load(Relaxed),
            "allocations_at_least_1_mib": LARGE.load(Relaxed),
            "note": "Warm decode, composition, resize, RGB565 conversion and writes. Physical mode includes GPIO/SPI completion, not optical scanout. Allocation volume, not peak memory."
        })
    );
    Ok(())
}
