# whisplay-driver

A Rust client and Linux hardware daemon for the Whisplay display. The daemon
owns the LCD, displays images and pixels, and plays GIFs independently of its clients.

## Client library

Add the client to a Rust application:

```sh
cargo add whisplay-driver
```

`Client` connects to a running daemon over a local Unix socket. It submits PNG,
JPEG and GIF files or RGB565 pixels, clears the screen, controls brightness and
the board's RGB light, and reads display state.

The client library needs no feature flags. Enable `daemon` to build `whisplayd`
and its hardware/media dependencies.

## Hardware daemon

```sh
cargo install whisplay-driver --features daemon --locked
whisplayd
```

Tested on a Raspberry Pi Zero 2 W with the 240×280 Whisplay LCD. The daemon needs
Linux access to `/dev/spidev0.0` and `/dev/gpiochip0`. Run `whisplayd --help` for
socket and simulation options; arrange service management separately if needed.

## Display behavior

The screen has one active animation. A new display submission replaces playback;
updates to a selected region leave surrounding pixels alone. Files are fitted
with their aspect ratio preserved. GIF timing and repeat settings come from the file.

Submissions return once the daemon owns and accepts the input. Callers can then
reuse pixel slices or remove source files. Playback continues after they disconnect.
Later playback failures are logged and available through `Client::state`.

See the [API documentation](https://docs.rs/whisplay-driver) for coordinates,
pixel format, input limits and error handling. For text, emoji and command-line
use, see [whisplay](https://crates.io/crates/whisplay).

Licensed under [Apache-2.0](LICENSE). See [NOTICE](NOTICE) for attribution.
