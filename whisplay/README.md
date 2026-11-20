# whisplay

Display text, color emoji, images and animations on Whisplay from the command line
or a Rust application. This crate renders text and arranges emoji with captions;
the hardware daemon handles image display and GIF playback.

## Command line

```sh
cargo install whisplay --locked
```

Start `whisplayd` from [whisplay-driver](https://crates.io/crates/whisplay-driver),
then send it content:

```sh
whisplay display text 'Ready'
whisplay display emoji '👀'
whisplay display text-and-emoji 'Looking into it' '👀'
whisplay display image --file photo.png
whisplay display video animation.gif
whisplay display clear
whisplay state
```

Commands return after acceptance; animations keep playing after the CLI exits.
The `video` command supports GIFs. Run `whisplay --help` or subcommand help for
regions, raw pixel input, brightness and other options.

## Fonts and emoji

Text uses a local font; emoji use local PNG or GIF assets. Set `WHISPLAY_FONT`
and `WHISPLAY_EMOJI_DIR` to choose them. Animated emoji play directly from GIFs.
Assets must be installed beforehand; display commands never download them.

The [configuration documentation](https://docs.rs/whisplay/latest/whisplay/#configuration)
describes defaults, asset filenames and a source for emoji collections.

## Rust library

`display_text` displays a message with an optional emoji using the same renderer
as the CLI. The crate also re-exports the driver's `Client` for image, animation
and pixel submissions. The library can be used without launching the CLI.

See the [API documentation](https://docs.rs/whisplay) for examples, text capacity
and the behavior of combined text/emoji submissions.

Licensed under [Apache-2.0](LICENSE). See [NOTICE](NOTICE) for attribution.
