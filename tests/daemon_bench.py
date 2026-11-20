"""Measure a Linux daemon playing a local looping GIF at its encoded frame delays."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import signal
import statistics
import subprocess
import tempfile
import time
from datetime import datetime, timezone

ROOT = Path(__file__).resolve().parents[1]
WARMUP_SECONDS = 3


def counters(path):
    values = {}
    for line in path.read_text().splitlines():
        name, separator, value = line.partition(':')
        fields = value.split()
        if separator and name.isidentifier() and fields and fields[0].isdigit():
            values[name] = int(fields[0])
    return values


def sample(pid):
    proc = Path('/proc') / str(pid)
    # Fields after the final ')' start at stat field 3; utime/stime are 14/15.
    stat = proc.joinpath('stat').read_text().rsplit(')', 1)[1].split()
    memory = counters(proc / 'smaps_rollup')
    status = counters(proc / 'status')
    return {
        'monotonic_seconds': time.monotonic(),
        'cpu_ticks': int(stat[11]) + int(stat[12]),
        'rss_kib': memory['Rss'],
        'pss_kib': memory['Pss'],
        'peak_rss_kib': status['VmHWM'],
        'swap_kib': status['VmSwap'],
        'threads': status['Threads'],
    }


def measure(pid, seconds):
    readings = [sample(pid)]
    deadline = time.monotonic() + seconds
    while (remaining := deadline - time.monotonic()) > 0:
        time.sleep(min(1, remaining))
        readings.append(sample(pid))
    first, last = readings[0], readings[-1]
    elapsed = last['monotonic_seconds'] - first['monotonic_seconds']
    return {
        'elapsed_seconds': elapsed,
        'cpu_percent_of_one_core': (last['cpu_ticks'] - first['cpu_ticks']) / os.sysconf('SC_CLK_TCK') / elapsed * 100,
        'rss_kib_median': statistics.median(row['rss_kib'] for row in readings),
        'rss_kib_max': max(row['rss_kib'] for row in readings),
        'pss_kib_median': statistics.median(row['pss_kib'] for row in readings),
        'peak_rss_kib': last['peak_rss_kib'],
        'swap_kib_max': max(row['swap_kib'] for row in readings),
        'samples': readings,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('gif', nargs='?', type=Path, default=ROOT / 'tests/fixtures/fry-money.gif')
    parser.add_argument('--hardware', action='store_true', help='Use the LCD; stop other display daemons first.')
    parser.add_argument('--full-screen', action='store_true', help='Use 240×280 instead of the 128×128 region.')
    parser.add_argument('--seconds', type=float, default=30, help='Playback measurement duration (default: 30).')
    args = parser.parse_args()
    if not Path('/proc/self/smaps_rollup').is_file():
        parser.error('requires Linux procfs with smaps_rollup')
    if not math.isfinite(args.seconds) or args.seconds <= 0:
        parser.error('--seconds must be positive and finite')
    source = args.gif.resolve(strict=True)
    profile = os.environ.get('WHISPLAY_PROFILE', 'release')
    daemon = ROOT / 'target' / profile / 'whisplayd'
    client = ROOT / 'target' / profile / 'whisplay'

    with tempfile.TemporaryDirectory(prefix='whisplay-daemon-bench-') as work:
        endpoint = Path(work) / 'display.sock'
        log = Path(work) / 'daemon.log'
        command = [str(daemon), '--socket', str(endpoint)]
        if not args.hardware:
            command.append('--simulate')
        with log.open('w') as stderr:
            process = subprocess.Popen(command, stdout=subprocess.DEVNULL, stderr=stderr)

        def cli(*arguments):
            result = subprocess.run([str(client), '--socket', str(endpoint), *arguments], check=True, capture_output=True, text=True, timeout=10)
            return json.loads(result.stdout)

        try:
            deadline = time.monotonic() + 10
            while not endpoint.exists():
                if process.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError('daemon startup failed: ' + log.read_text())
                time.sleep(0.02)
            cli('display', 'clear')
            time.sleep(1)
            idle = measure(process.pid, WARMUP_SECONDS)
            region = [] if args.full_screen else ['--columns', '56..184', '--rows', '12..140']
            cli('display', 'video', str(source), *region)
            time.sleep(WARMUP_SECONDS)
            if cli('state') != {'state': 'playing', 'error': None}:
                raise RuntimeError('benchmark requires a working, looping GIF')
            playing = measure(process.pid, args.seconds)
            state = cli('state')
            if state != {'state': 'playing', 'error': None}:
                raise RuntimeError('playback failed or ended: ' + json.dumps(state))
        finally:
            if process.poll() is None:
                process.send_signal(signal.SIGTERM)
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
                raise
        if process.returncode != 0 or log.read_text():
            raise RuntimeError(f'daemon exited {process.returncode}: {log.read_text()}')

    paths = [source, daemon, client, Path(__file__).resolve(), ROOT / 'Cargo.lock', *(ROOT / 'whisplay-driver/src').rglob('*.rs')]
    print(json.dumps({
        'recorded_at_utc': datetime.now(timezone.utc).isoformat(),
        'host': platform.node(), 'platform': platform.platform(),
        'hardware': 'physical' if args.hardware else 'simulated',
        'display_region': [240, 280] if args.full_screen else [128, 128],
        'warmup_seconds': WARMUP_SECONDS,
        'requested_seconds': args.seconds,
        'clock_ticks_per_second': os.sysconf('SC_CLK_TCK'),
        'idle': idle, 'playing': playing, 'state': state,
        'sha256': {str(path.relative_to(ROOT) if path.is_relative_to(ROOT) else path): hashlib.sha256(path.read_bytes()).hexdigest() for path in sorted(paths)},
        'measurement': 'Daemon CPU across threads, percent of one core; RSS/PSS from smaps_rollup. Peak RSS is the kernel-reported process-lifetime high-water mark. Original GIF delays, no frame-rate override or observed-FPS measurement. CLI/monitor CPU and memory excluded.',
    }, indent=2))


if __name__ == '__main__':
    main()
