//! LCD transactions and direct brightness outputs. No media or power policy.
//!
//! LCD initialization is adapted from PiSugar Whisplay's Apache-2.0 Python driver.
//! This Rust implementation uses gpio-cdev/spidev; see the project's NOTICE.

use gpio_cdev::{Chip, LineHandle, LineRequestFlags};
use spidev::{SpiModeFlags, Spidev, SpidevOptions};
use std::{
    io::{self, Write},
    ops::Range,
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::Duration,
};
use whisplay_driver::{shared::pixel_bytes, HEIGHT, WIDTH};

pub(super) enum Hardware {
    Physical(Board),
    Simulated(Vec<u8>),
}

pub(super) struct Board {
    lcd: Lcd,
    backlight: Pwm,
    led: [Pwm; 3],
}

struct Lcd {
    spi: Spidev,
    dc: LineHandle,
    _reset: LineHandle,
}

impl Hardware {
    pub(super) fn open(simulate: bool) -> io::Result<Self> {
        if simulate {
            let pixels = vec![0; usize::from(WIDTH) * usize::from(HEIGHT) * 2];
            return Ok(Self::Simulated(pixels));
        }
        let mut chip = Chip::new("/dev/gpiochip0").map_err(io::Error::other)?;
        let mut pin = |number| {
            chip.get_line(number)
                .and_then(|line| line.request(LineRequestFlags::OUTPUT, 1, "whisplayd"))
                .map_err(io::Error::other)
        };
        let dc = pin(27)?;
        let reset = pin(4)?;
        for (level, delay) in [(1, 100), (0, 100), (1, 120)] {
            reset.set_value(level).map_err(io::Error::other)?;
            thread::sleep(Duration::from_millis(delay));
        }
        let mut spi = Spidev::open("/dev/spidev0.0")?;
        spi.configure(
            &SpidevOptions::new()
                .bits_per_word(8)
                .max_speed_hz(100_000_000)
                .mode(SpiModeFlags::SPI_MODE_0)
                .build(),
        )?;
        let mut lcd = Lcd {
            spi,
            dc,
            _reset: reset,
        };
        lcd.command(0x11, &[])?;
        thread::sleep(Duration::from_millis(120));
        // Panel initialization and gamma values from the validated Whisplay board.
        for (command, data) in [
            (0x36, &[0xC0][..]),
            (0x3A, &[0x05]),
            (0xB2, &[0x0C, 0x0C, 0x00, 0x33, 0x33]),
            (0xB7, &[0x35]),
            (0xBB, &[0x32]),
            (0xC2, &[0x01]),
            (0xC3, &[0x15]),
            (0xC4, &[0x20]),
            (0xC6, &[0x0F]),
            (0xD0, &[0xA4, 0xA1]),
            (
                0xE0,
                &[
                    0xD0, 0x08, 0x0E, 0x09, 0x09, 0x05, 0x31, 0x33, 0x48, 0x17, 0x14, 0x15, 0x31,
                    0x34,
                ],
            ),
            (
                0xE1,
                &[
                    0xD0, 0x08, 0x0E, 0x09, 0x09, 0x15, 0x31, 0x33, 0x48, 0x17, 0x14, 0x15, 0x31,
                    0x34,
                ],
            ),
            (0x21, &[]),
            (0x29, &[]),
        ] {
            lcd.command(command, data)?;
        }
        lcd.write_pixels(
            &vec![0; usize::from(WIDTH) * usize::from(HEIGHT) * 2],
            0..WIDTH,
            0..HEIGHT,
        )?;
        // Active-low GPIO: endpoint brightness blocks instead of running a timer.
        let mut output = |number, period, maximum| {
            Pwm::new(pin(number)?, Duration::from_millis(period), maximum)
        };
        let board = Board {
            lcd,
            backlight: output(22, 1, 100)?,
            led: [
                output(25, 10, 255)?,
                output(24, 10, 255)?,
                output(23, 10, 255)?,
            ],
        };
        board.backlight.set(100)?;
        Ok(Self::Physical(board))
    }

    /// Write a region without changing the surrounding pixels.
    pub(super) fn write_pixels(
        &mut self,
        pixels: &[u8],
        columns: Range<u16>,
        rows: Range<u16>,
    ) -> io::Result<()> {
        if pixel_bytes(&columns, &rows)? != pixels.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "incorrect pixel length",
            ));
        }
        match self {
            Self::Physical(board) => board.lcd.write_pixels(pixels, columns, rows),
            Self::Simulated(frame) => {
                let row_bytes = usize::from(columns.end - columns.start) * 2;
                for (row, data) in rows.zip(pixels.chunks_exact(row_bytes)) {
                    let start =
                        (usize::from(row) * usize::from(WIDTH) + usize::from(columns.start)) * 2;
                    frame[start..start + row_bytes].copy_from_slice(data);
                }
                Ok(())
            }
        }
    }

    pub(super) fn set_backlight(&self, percent: u8) -> io::Result<()> {
        if percent > 100 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "backlight must be in 0..=100",
            ));
        }
        if let Self::Physical(board) = self {
            board.backlight.set(u16::from(percent))?;
        }
        Ok(())
    }

    pub(super) fn set_led(&self, rgb: [u8; 3]) -> io::Result<()> {
        if let Self::Physical(board) = self {
            for (output, value) in board.led.iter().zip(rgb) {
                output.set(u16::from(value))?;
            }
        }
        Ok(())
    }
}

impl Lcd {
    /// Translate half-open screen ranges into the controller's inclusive window.
    fn write_pixels(
        &mut self,
        pixels: &[u8],
        columns: Range<u16>,
        rows: Range<u16>,
    ) -> io::Result<()> {
        let x0 = columns.start.to_be_bytes();
        let x1 = (columns.end - 1).to_be_bytes();
        // The visible panel occupies controller rows 20..=299.
        let y0 = (rows.start + 20).to_be_bytes();
        let y1 = (rows.end + 19).to_be_bytes();
        self.command(0x2A, &[x0[0], x0[1], x1[0], x1[1]])?;
        self.command(0x2B, &[y0[0], y0[1], y1[0], y1[1]])?;
        self.command(0x2C, &[])?;
        self.dc.set_value(1).map_err(io::Error::other)?;
        // Match the Pi's spidev transfer-size limit.
        for chunk in pixels.chunks(4096) {
            self.spi.write_all(chunk)?;
        }
        Ok(())
    }

    fn command(&mut self, command: u8, data: &[u8]) -> io::Result<()> {
        self.dc.set_value(0).map_err(io::Error::other)?;
        self.spi.write_all(&[command])?;
        if !data.is_empty() {
            self.dc.set_value(1).map_err(io::Error::other)?;
            self.spi.write_all(data)?;
        }
        Ok(())
    }
}

struct Pwm {
    state: Arc<(Mutex<Option<u16>>, Condvar)>,
    worker: Option<JoinHandle<io::Result<()>>>,
}

impl Pwm {
    fn new(line: LineHandle, period: Duration, maximum: u16) -> io::Result<Self> {
        let state = Arc::new((Mutex::new(Some(0)), Condvar::new()));
        let shared = Arc::clone(&state);
        let worker = thread::Builder::new()
            .name("whisplay-pwm".into())
            .spawn(move || {
                let result = pwm(&line, period, maximum, &shared);
                let off = line.set_value(1).map_err(io::Error::other);
                if let Err(error) = &result {
                    eprintln!("brightness output: {error}");
                }
                result.and(off)
            })?;
        Ok(Self {
            state,
            worker: Some(worker),
        })
    }

    fn set(&self, value: u16) -> io::Result<()> {
        if self.worker.as_ref().is_some_and(JoinHandle::is_finished) {
            return Err(io::Error::other("brightness worker has stopped"));
        }
        let (lock, changed) = &*self.state;
        *lock
            .lock()
            .map_err(|_| io::Error::other("brightness lock poisoned"))? = Some(value);
        changed.notify_one();
        Ok(())
    }
}

impl Drop for Pwm {
    fn drop(&mut self) {
        let (lock, changed) = &*self.state;
        *lock.lock().unwrap_or_else(|error| error.into_inner()) = None;
        changed.notify_one();
        if let Some(worker) = self.worker.take() {
            if let Err(error) = worker
                .join()
                .unwrap_or_else(|_| Err(io::Error::other("brightness worker panicked")))
            {
                eprintln!("brightness shutdown: {error}");
            }
        }
    }
}

fn pwm(
    line: &LineHandle,
    period: Duration,
    maximum: u16,
    state: &(Mutex<Option<u16>>, Condvar),
) -> io::Result<()> {
    let (lock, changed) = state;
    let mut current = lock
        .lock()
        .map_err(|_| io::Error::other("brightness lock poisoned"))?;
    while let Some(value) = *current {
        if value == 0 || value == maximum {
            line.set_value(u8::from(value == 0))
                .map_err(io::Error::other)?;
            current = changed
                .wait_while(current, |target| *target == Some(value))
                .map_err(|_| io::Error::other("brightness lock poisoned"))?;
            continue;
        }
        let off = period.mul_f64(f64::from(maximum - value) / f64::from(maximum));
        line.set_value(1).map_err(io::Error::other)?;
        current = changed
            .wait_timeout_while(current, off, |target| *target == Some(value))
            .map_err(|_| io::Error::other("brightness lock poisoned"))?
            .0;
        if *current != Some(value) {
            continue;
        }
        line.set_value(0).map_err(io::Error::other)?;
        current = changed
            .wait_timeout_while(current, period - off, |target| *target == Some(value))
            .map_err(|_| io::Error::other("brightness lock poisoned"))?
            .0;
    }
    Ok(())
}
