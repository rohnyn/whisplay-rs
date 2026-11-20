# Whisplay

A Rust driver, daemon and CLI for the Whisplay display. Show text, emoji, images
and animations from a script, a Rust application or the command line.

## What it supports

- PNG and JPEG images, plus GIF playback with the file's timing and loop settings.
- Text, color emoji and animated emoji using local fonts and assets.
- Updates to part of the screen, including direct pixel input.
- Screen brightness control.

This project covers the display. It does not provide audio or button input.

## Getting it

[Download binaries](https://github.com/rohnyn/whisplay-rs/releases) for 64-bit
Linux ARM, or install from crates.io:

```sh
cargo install whisplay --locked
cargo install whisplay-driver --features daemon --locked
```

## Hardware support

Tested on a Raspberry Pi Zero 2 W with a 240×280 Whisplay LCD. The daemon needs
Linux SPI and GPIO access; other board combinations have not been validated here.
See the [daemon documentation](https://github.com/rohnyn/whisplay-rs/blob/main/whisplay-driver/src/bin/whisplayd/main.rs) for hardware access requirements.

## How it works

`whisplayd` owns the screen and handles image display and animation playback.
The CLI and Rust client send it content over a local socket. Commands return
after the content is accepted, and playback continues after the caller exits.

The display has one active animation at a time. A new image, animation or pixel
update replaces it. Updates to a selected region leave the rest of the screen alone.

## Usage

With `whisplayd` running:

```sh
whisplay display text 'Ready'
whisplay display emoji '👀'
whisplay display text-and-emoji 'Looking into it' '👀'
whisplay display image --file photo.png
whisplay display video animation.gif
whisplay display clear
```

Fonts and emoji assets are supplied separately. The
[presentation crate docs](https://docs.rs/whisplay) cover their configuration.
Use `whisplay --help` and subcommand help for available options.

## Rust libraries

- [`whisplay-driver`](https://docs.rs/whisplay-driver) provides the display client and hardware daemon, including image and GIF playback.
- [`whisplay`](https://docs.rs/whisplay) provides the CLI and functions for displaying text and emoji.

API details, input limits and error handling are documented alongside the public functions.

## Performance and validation

The [results](https://github.com/rohnyn/whisplay-rs/blob/main/reports/README.md) include CPU and memory measurements on the
Pi Zero 2 W with real LCD output, plus an extended run that switches GIFs and
clears the display repeatedly. The report identifies the workload and tested build.

See [tests and benchmarks](https://github.com/rohnyn/whisplay-rs/blob/main/tests/README.md) for the checks and measurement methods.

## License

Code is licensed under [Apache-2.0](https://github.com/rohnyn/whisplay-rs/blob/main/LICENSE). See [NOTICE](https://github.com/rohnyn/whisplay-rs/blob/main/NOTICE) for attribution
and third-party media.
