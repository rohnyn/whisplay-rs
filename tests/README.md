# Validation and benchmarks

Run commands from the workspace root. Stop other display daemons before running
physical hardware checks.

| Source | Purpose | Output |
| --- | --- | --- |
| [cli_smoke.py](cli_smoke.py) | CLI/socket integration, input ownership, rendering and shutdown | Text log; nonzero exit on failure |
| [emoji_latency.py](emoji_latency.py) | Six emoji/text acceptance-latency samples | JSON; diagnostics on stderr |
| [daemon_bench.py](daemon_bench.py) | Idle and GIF-paced daemon CPU, resident/proportional memory and peak RSS on Linux | JSON; diagnostics on stderr |
| [daemon_soak.py](daemon_soak.py) | One-daemon playback, GIF switching, clearing and memory-growth checks | JSON samples, request logs and summary |
| [playback_bench.rs](../whisplay-driver/examples/playback_bench.rs) | Warm GIF decoding/rendering/writing time and allocation volume, with simulated or physical output | JSON; diagnostics on stderr |
| [fixtures/fullscreen.gif](fixtures/fullscreen.gif) | Native 240×280 moving-bar/border fixture for screen inspection | Input asset |
| [fixtures/fry-money.gif](fixtures/fry-money.gif) | Futurama animation for region and full-screen playback benchmarks | Input asset |
| [fixtures/fry-money-68.gif](fixtures/fry-money-68.gif) | Re-encoded Fry sequence repeated twice; exceeds the full-screen frame-cache budget | Input asset |

Keep generated output under `target/reports/`, which is already ignored by Git.
Use a separate directory per run; a validation summary lives in
[reports/](../reports/README.md).

## Build and checks

```sh
WHISPLAY_REPORT_DIR="target/reports/$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$WHISPLAY_REPORT_DIR"

cargo fmt --all -- --check >"$WHISPLAY_REPORT_DIR/format.log" 2>&1
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings >"$WHISPLAY_REPORT_DIR/clippy.log" 2>&1
cargo test --workspace --all-features --locked >"$WHISPLAY_REPORT_DIR/unit-tests.log" 2>&1
cargo build --release --workspace --all-features --locked >"$WHISPLAY_REPORT_DIR/build.log" 2>&1
```

## CLI and acceptance latency

Run on the Pi, or select an installed font with `WHISPLAY_FONT` on another machine.
The latency check needs a preinstalled `1f440.gif` in `WHISPLAY_EMOJI_DIR` (default
`$HOME/.local/share/whisplay/emoji`). It downloads nothing. The smoke check creates
its own temporary image/emoji fixtures. Both scripts start and stop their own
daemon using a temporary socket.

```sh
WHISPLAY_PROFILE=release python3 tests/cli_smoke.py >"$WHISPLAY_REPORT_DIR/cli-simulated.log" 2>&1
WHISPLAY_PROFILE=release WHISPLAY_REAL_HARDWARE=1 python3 tests/cli_smoke.py >"$WHISPLAY_REPORT_DIR/cli-hardware.log" 2>&1
WHISPLAY_REAL_HARDWARE=1 python3 tests/emoji_latency.py >"$WHISPLAY_REPORT_DIR/emoji-latency.json" 2>"$WHISPLAY_REPORT_DIR/emoji-latency.stderr.log"
```

Omit `WHISPLAY_REAL_HARDWARE=1` to simulate hardware. The latency check measures
acceptance, not LCD transfer completion or optical scanout. For its known asset,
the current regression budgets are 200 ms for the first call and 150 ms for later
calls. Those are workload-specific bounds, not an all-media performance guarantee.

## Playback benchmark

Provide a GIF that loops indefinitely. The benchmark warms two complete passes,
then measures five passes. It bypasses frame delays and writes every frame.
Output defaults to simulated writes; `--hardware` uses the actual LCD and includes
GPIO/SPI write completion in the timings. Stop the daemon before physical runs:
the benchmark needs exclusive hardware access. Each physical run initializes the
screen and turns off its backlight/LED outputs when it exits.

Eligible repeating GIFs fill the bounded cache during warmup, so their measured
passes exercise cached replay. Over-budget GIFs continue decoding and rendering.
The benchmark's generic decode/render/write labels include either path.
Allocator counters measure requested allocation volume/calls, not peak memory;
cache construction during preparation/warmup is outside those counters.
`prepare_ms` measures input preparation and first-frame rendering before writing;
it excludes hardware initialization. `milliseconds_per_frame` measures warm
playback/write time. This does not measure socket/client latency, paced
daemon CPU usage, optical scanout or synchronized screen FPS.

```sh
cargo run --release --locked -p whisplay-driver --features daemon --example playback_bench -- /path/to/looping.gif >"$WHISPLAY_REPORT_DIR/playback-region.json" 2>"$WHISPLAY_REPORT_DIR/playback-region.stderr.log"
cargo run --release --locked -p whisplay-driver --features daemon --example playback_bench -- /path/to/looping.gif --full-screen >"$WHISPLAY_REPORT_DIR/playback-fullscreen.json" 2>"$WHISPLAY_REPORT_DIR/playback-fullscreen.stderr.log"
```

The default viewport is 128×128; `--full-screen` uses 240×280.

The included Fry GIF is 498×278, with 34 frames at 50 ms each, looping indefinitely.
It is preserved without resizing or transcoding from
[Tenor](https://tenor.com/view/futurama-philip-j-fry-shut-up-and-take-my-money-take-my-money-dollars-gif-4970136)
([original GIF](https://media1.tenor.com/m/_ongt_zYBPkAAAAd/futurama-philip-j-fry.gif)).
Its SHA-256 is `0cb310bde190816dec1b6885b8855827c9cda70b83de017e251d7e43d38e669b`.
Both viewports preserve aspect ratio and include black margins.

```sh
cargo run --release --locked -p whisplay-driver --features daemon --example playback_bench -- tests/fixtures/fry-money.gif >"$WHISPLAY_REPORT_DIR/fry-region.json" 2>"$WHISPLAY_REPORT_DIR/fry-region.stderr.log"
cargo run --release --locked -p whisplay-driver --features daemon --example playback_bench -- tests/fixtures/fry-money.gif --full-screen >"$WHISPLAY_REPORT_DIR/fry-fullscreen.json" 2>"$WHISPLAY_REPORT_DIR/fry-fullscreen.stderr.log"
cargo run --release --locked -p whisplay-driver --features daemon --example playback_bench -- tests/fixtures/fry-money.gif --hardware >"$WHISPLAY_REPORT_DIR/fry-region-hardware.json" 2>"$WHISPLAY_REPORT_DIR/fry-region-hardware.stderr.log"
cargo run --release --locked -p whisplay-driver --features daemon --example playback_bench -- tests/fixtures/fry-money.gif --full-screen --hardware >"$WHISPLAY_REPORT_DIR/fry-fullscreen-hardware.json" 2>"$WHISPLAY_REPORT_DIR/fry-fullscreen-hardware.stderr.log"
```

For visual inspection, with a daemon already running:

```sh
whisplay display video tests/fixtures/fullscreen.gif
whisplay display video tests/fixtures/fry-money.gif
whisplay display clear
```

Automated checks do not establish whether someone observed flicker or tearing.

## Paced daemon benchmark

This runs the actual daemon and submits the GIF through the CLI/socket interface.
It records a three-second idle baseline, allows three seconds of playback warmup,
then samples CPU and memory once per second for 30 seconds. The GIF's own frame
delays remain in effect; Fry requests 20 frames/second. This does not measure
observed frame rate or optical scanout.

CPU includes the daemon's threads and kernel CPU charged to that process, with
100% meaning one fully occupied core. CLI/monitor resources are excluded. RSS and
PSS come from [Linux `smaps_rollup`](https://docs.kernel.org/filesystems/proc.html);
PSS apportions shared pages. Peak RSS is the
kernel-reported process-lifetime high-water mark, including startup. Raw samples
and source/binary/fixture hashes are retained. No third-party Python packages are
needed. `--seconds` changes the playback measurement duration.

Stop other display daemons before hardware runs. Each invocation owns and stops
its daemon; it does not alter or restart unrelated processes. Hardware runs show
the animation and turn the brightness outputs off on exit.

```sh
python3 tests/daemon_bench.py >"$WHISPLAY_REPORT_DIR/daemon-region-simulated.json" 2>"$WHISPLAY_REPORT_DIR/daemon-region-simulated.stderr.log"
python3 tests/daemon_bench.py --full-screen >"$WHISPLAY_REPORT_DIR/daemon-fullscreen-simulated.json" 2>"$WHISPLAY_REPORT_DIR/daemon-fullscreen-simulated.stderr.log"
python3 tests/daemon_bench.py --hardware >"$WHISPLAY_REPORT_DIR/daemon-region-physical.json" 2>"$WHISPLAY_REPORT_DIR/daemon-region-physical.stderr.log"
python3 tests/daemon_bench.py --full-screen --hardware >"$WHISPLAY_REPORT_DIR/daemon-fullscreen-physical.json" 2>"$WHISPLAY_REPORT_DIR/daemon-fullscreen-physical.stderr.log"
```

## Extended playback and memory checks

The soak runner uses one daemon process throughout: an idle baseline, ten minutes
of continuous playback, fifteen minutes of GIF switching and clearing, three
minutes of rapid replacements, then two minutes cleared and idle. It checks
resident memory, swap, thread count, file descriptors and daemon errors.

It uses the included Fry and moving-bar GIFs. The 68-frame fixture exceeds the
frame-cache budget; the runner derives a 62-frame near-limit fixture without
decoding or re-encoding it. No
downloads, optional emoji assets or third-party Python packages are required.

After building the workspace, run a short entry-point/cleanup check or the full
soak. Each invocation creates its own daemon and stops it on exit. Hardware mode
requires exclusive display access; the script does not stop unrelated daemons.

```sh
python3 tests/daemon_soak.py --quick --output-dir "$WHISPLAY_REPORT_DIR/soak-quick"
python3 tests/daemon_soak.py --hardware --output-dir "$WHISPLAY_REPORT_DIR/soak-hardware"
```

Omit `--hardware` for simulated output. Quick mode takes roughly 40 seconds and
does not enforce the full run's long-term growth comparisons. The default full
run takes about 31 minutes. Output includes `soak-summary.json`, raw memory
samples, submitted commands, clear checkpoints, daemon logs and input/build hashes.
The report directory must be new; existing results are never overwritten.

The full run checks for at most 1 MiB RSS growth during continuous playback and
2 MiB growth between matching cleared-state windows after warmup. A 32 MiB RSS /
kernel-high-water guard applies to this test corpus, alongside zero swap and
stable resource counts. These are test criteria, not production memory limits.
Freed allocator storage can remain resident; a finite run cannot prove indefinite
leak freedom or establish visual absence of tearing.

The original recorded soak used the local Noto eyes animation as the additional
GIF. To repeat those inputs, supply that asset explicitly:

```sh
python3 tests/daemon_soak.py --hardware --extra-gif "$WHISPLAY_EMOJI_DIR/1f440.gif" --output-dir "$WHISPLAY_REPORT_DIR/soak-eyes"
```

The expected eyes asset SHA-256 is
`cd74f25ba724237765bc5d481a4a1c20299a4cd85ee8949b78123a0cb729a1b0`.

## CPU profiling by function and thread

The `profiling` Cargo profile retains release optimizations plus function/source
symbols. There is no profiler library or instrumentation in the daemon. Linux
`perf` does the sampling externally; installing that tool is a separate, explicit
machine setup step. It needs DWARF unwinding support for the commands below.

Stop any existing hardware daemon. In one terminal, build and run the profiled
daemon in the foreground; Ctrl-C stops it and releases the hardware:

```sh
cargo build --profile profiling --locked -p whisplay-driver --features daemon --bin whisplayd
./target/profiling/whisplayd --socket /tmp/whisplay-profile.sock
```

In another terminal, use the existing release CLI, allow playback to warm up,
then sample 45 seconds. Set `WHISPLAY_REPORT_DIR` as above first. Only one
`whisplayd` should be running when selecting its PID:

```sh
./target/release/whisplay --socket /tmp/whisplay-profile.sock display video tests/fixtures/fry-money.gif
WHISPLAY_PROFILE_PID=$(pgrep -n -x whisplayd)
ps -T -p "$WHISPLAY_PROFILE_PID" -o pid,tid,comm >"$WHISPLAY_REPORT_DIR/threads.txt"
sleep 3
perf record -e cpu-clock:u -F 99 --call-graph dwarf,8192 -p "$WHISPLAY_PROFILE_PID" -o "$WHISPLAY_REPORT_DIR/perf.data" -- sleep 45
perf report --stdio --no-children --call-graph none --sort comm,pid,dso,symbol -n -i "$WHISPLAY_REPORT_DIR/perf.data" >"$WHISPLAY_REPORT_DIR/functions.txt"
perf report --stdio --children -i "$WHISPLAY_REPORT_DIR/perf.data" >"$WHISPLAY_REPORT_DIR/callgraph.txt"
perf script -F comm,pid,tid,time,event,ip,sym,dso -i "$WHISPLAY_REPORT_DIR/perf.data" >"$WHISPLAY_REPORT_DIR/stacks.txt"
```

`perf report -i ...` opens the interactive function/call-stack view and supports
instruction/source annotation. Thread IDs and names appear in the saved task
list and raw stacks. The source-line sort key `srcline` is also available.
See the [perf record documentation](https://man7.org/linux/man-pages/man1/perf-record.1.html)
and [Cargo profile settings](https://doc.rust-lang.org/cargo/reference/profiles.html).

These are statistical on-CPU samples, not exact timings of every function call.
`cpu-clock:u` excludes kernel execution and blocked/sleeping time. Self percentages
show where samples landed; inclusive/children percentages include callees and
must not be added together. Retain the profiled executable and source revision
alongside the raw data for later symbolization. Profiling can perturb execution;
use the separate paced resource benchmark for normal CPU/memory measurements.
