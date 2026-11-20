# Whisplay

A Rust driver, daemon and CLI for the Whisplay display. Show messages, color emoji,
images and animations from scripts or Rust applications.

## What it supports

- PNG and JPEG images, plus GIF playback with the file's timing and loop settings.
- Text, color emoji and animated emoji using local fonts and assets.
- Updates to part of the screen, including direct RGB565 pixel input.
- Screen brightness control.

This project covers the display. Audio and button input are separate.

## Two crates

| Crate | Purpose | Documentation |
| --- | --- | --- |
| [`whisplay-driver`](whisplay-driver/README.md) | Owns the LCD and handles images, pixels and GIF playback. Includes a Rust client and the `whisplayd` daemon. | [API](https://docs.rs/whisplay-driver) |
| [`whisplay`](whisplay/README.md) | Provides the CLI and a Rust library for text and emoji presentation. | [API](https://docs.rs/whisplay) |

The daemon owns the screen. Clients submit content over a local socket and can
exit while playback continues. A new display submission replaces the active
animation; region updates retain the rest of the screen.

Start with the crate READMEs for installation, usage and configuration.
[Binary downloads](https://github.com/rohnyn/whisplay-rs/releases) include both
executables for 64-bit ARM Linux.

## Hardware support

Tested on a Raspberry Pi Zero 2 W with a 240×280 Whisplay LCD. The daemon requires
Linux SPI and GPIO access. Other board combinations have not been validated here.

## Performance and validation

The [results](reports/README.md) cover CPU and memory use with real LCD output,
including an extended run switching GIFs and clearing the display. The report
identifies the workload and tested build.

See [tests and benchmarks](tests/README.md) to reproduce the checks.

## License

Code is licensed under [Apache-2.0](LICENSE). See [NOTICE](NOTICE) for attribution
and third-party media.
