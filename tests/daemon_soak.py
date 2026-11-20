"""Exercise GIF replacement, clearing and memory bounds in one Linux daemon."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import statistics
import subprocess
import tempfile
import time
from datetime import datetime, timezone

from daemon_bench import ROOT, sample

RSS_LIMIT_KIB = 32 * 1024
GROWTH_LIMIT_KIB = 2 * 1024


def frame_ends(data):
    """Locate image-block ends in the bundled GIF, without decoding its pixels."""
    position = 13 + (3 * (1 << ((data[10] & 7) + 1)) if data[10] & 128 else 0)
    ends = []
    while data[position] != 0x3b:
        tag = data[position]
        if tag == 0x21:
            position += 2
        elif tag == 0x2c:
            packed = data[position + 9]
            position += 10 + (3 * (1 << ((packed & 7) + 1)) if packed & 128 else 0) + 1
        else:
            raise ValueError('unexpected GIF block in the bundled fixture')
        while (size := data[position]):
            position += size + 1
        position += 1
        if tag == 0x2c:
            ends.append(position)
    return ends


def summarize(readings, clear_points):
    phases = {}
    for name in ['baseline', 'continuous', 'switching', 'rapid', 'cleared-idle']:
        rows = [row for row in readings if row['phase'] == name]
        if not rows:
            continue
        first, last = rows[0], rows[-1]
        seconds = last['monotonic_seconds'] - first['monotonic_seconds']
        phases[name] = {
            'seconds': seconds,
            'samples': len(rows),
            'rss_kib_median': statistics.median(row['rss_kib'] for row in rows),
            'rss_kib_min': min(row['rss_kib'] for row in rows),
            'rss_kib_max': max(row['rss_kib'] for row in rows),
            'pss_kib_median': statistics.median(row['pss_kib'] for row in rows),
            'cpu_percent_of_one_core': (last['cpu_ticks'] - first['cpu_ticks'])
                / os.sysconf('SC_CLK_TCK') / max(seconds, 1e-9) * 100,
        }
    growth = {}
    continuous = [row for row in readings if row['phase'] == 'continuous']
    if continuous:
        start = continuous[0]['elapsed_seconds']
        end = continuous[-1]['elapsed_seconds']
        early = [row['rss_kib'] for row in continuous
                 if 60 <= row['elapsed_seconds'] - start < 120]
        late = [row['rss_kib'] for row in continuous if row['elapsed_seconds'] >= end - 60]
        if early and late:
            growth['continuous_kib'] = statistics.median(late) - statistics.median(early)
    for kind in ['switching', 'rapid']:
        points = [row for row in clear_points if row['kind'] == kind]
        if len(points) < 20:
            continue
        if kind == 'switching':
            points = points[10:]
        else:
            points = [row for row in points
                      if row['elapsed_seconds'] - points[0]['elapsed_seconds'] >= 30]
        if len(points) >= 20:
            growth[kind + '_clear_kib'] = (
                statistics.median(row['rss_kib'] for row in points[-10:])
                - statistics.median(row['rss_kib'] for row in points[:10])
            )
    return phases, growth


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--hardware', action='store_true', help='Use the LCD; stop other display daemons first.')
    parser.add_argument('--quick', action='store_true', help='Run a roughly 40-second smoke check, not the full memory soak.')
    parser.add_argument('--extra-gif', type=Path, default=ROOT / 'tests/fixtures/fullscreen.gif',
                        help='Additional looping GIF; defaults to the included moving-bar fixture.')
    parser.add_argument('--output-dir', type=Path,
                        default=ROOT / 'target/reports' / datetime.now(timezone.utc).strftime('soak-%Y%m%dT%H%M%SZ'),
                        help='New directory for samples, request logs and summary JSON.')
    args = parser.parse_args()
    if not Path('/proc/self/smaps_rollup').is_file():
        parser.error('requires Linux procfs with smaps_rollup')
    profile = os.environ.get('WHISPLAY_PROFILE', 'release')
    daemon, client = (ROOT / 'target' / profile / name for name in ['whisplayd', 'whisplay'])
    fry = ROOT / 'tests/fixtures/fry-money.gif'
    fallback = ROOT / 'tests/fixtures/fry-money-68.gif'
    extra = args.extra_gif.resolve(strict=True)
    for path in [daemon, client, fry, fallback]:
        if not path.is_file():
            parser.error(f'missing {path}; build the workspace with the daemon feature first')
    report = args.output_dir.resolve()
    report.mkdir(parents=True, exist_ok=False)
    scale = 0.02 if args.quick else 1
    data = fallback.read_bytes()
    ends = frame_ends(data)
    if len(ends) != 68:
        raise ValueError('streaming fixture must have 68 frames')
    near = report / 'fry-62-frames.gif'
    near.write_bytes(data[:ends[61]] + b'\x3b')

    def save(name, value):
        (report / name).write_text(json.dumps(value, indent=2) + '\n')

    paths = [fry, extra, fallback, near, daemon, client, Path(__file__).resolve(),
             ROOT / 'tests/daemon_bench.py', ROOT / 'Cargo.lock',
             *(ROOT / 'whisplay-driver/src').rglob('*.rs')]
    provenance = {
        'started_at_utc': datetime.now(timezone.utc).isoformat(),
        'host': os.uname().nodename,
        'hardware': args.hardware,
        'quick': args.quick,
        'sha256': {str(path.relative_to(ROOT) if path.is_relative_to(ROOT) else path):
                   hashlib.sha256(path.read_bytes()).hexdigest() for path in sorted(set(paths))},
        'cache_budget_bytes': 8 * 1024 * 1024,
        'clock_ticks_per_second': os.sysconf('SC_CLK_TCK'),
        'thresholds': {
            'rss_and_kernel_peak_kib': RSS_LIMIT_KIB,
            'steady_growth_kib': 1024,
            'same_workload_clear_growth_kib': GROWTH_LIMIT_KIB,
            'max_swap_kib': 0,
        },
        'schedule_seconds': {name: seconds * scale for name, seconds in
                             [('baseline', 30), ('continuous', 600), ('switching', 900),
                              ('rapid', 180), ('cleared_idle', 120)]},
    }
    readings, events, clear_points = [], [], []
    phase, asset, cycle = 'setup', 'blank', 0
    started = time.monotonic()
    last_sample = last_heartbeat = 0
    baseline_threads = baseline_fds = None
    print(f'Report: {report}', flush=True)
    with tempfile.TemporaryDirectory(prefix='whisplay-soak-') as directory:
        endpoint = Path(directory) / 'display.sock'
        log = report / 'soak-daemon.log'
        command = [str(daemon), '--socket', str(endpoint)]
        if not args.hardware:
            command.append('--simulate')
        with log.open('w') as stderr:
            process = subprocess.Popen(command, stdout=subprocess.DEVNULL, stderr=stderr)
        provenance['daemon_pid'] = process.pid
        save('soak-provenance.json', provenance)

        def tick(force=False):
            nonlocal last_sample, last_heartbeat
            now = time.monotonic()
            if process.poll() is not None:
                raise RuntimeError('daemon exited during soak: ' + log.read_text())
            if log.stat().st_size:
                raise RuntimeError('daemon logged: ' + log.read_text())
            if force or now - last_sample >= 1:
                row = sample(process.pid)
                row.update(elapsed_seconds=now - started, phase=phase, asset=asset, cycle=cycle,
                           fds=len(list(Path(f'/proc/{process.pid}/fd').iterdir())))
                readings.append(row)
                last_sample = now
                if max(row['rss_kib'], row['peak_rss_kib']) > RSS_LIMIT_KIB:
                    raise RuntimeError('resident memory exceeded the 32 MiB test bound')
                if row['swap_kib']:
                    raise RuntimeError('daemon used swap')
                if baseline_threads is not None and row['threads'] != baseline_threads:
                    raise RuntimeError('thread count changed')
                if baseline_fds is not None and row['fds'] > baseline_fds + 2:
                    raise RuntimeError('file descriptor count grew')
            if now - last_heartbeat >= 60 and readings:
                latest = readings[-1]
                print(json.dumps({
                    'phase': phase, 'elapsed_seconds': round(now - started), 'cycle': cycle,
                    'rss_mib': latest['rss_kib'] / 1024,
                    'maximum_observed_rss_mib': max(row['rss_kib'] for row in readings) / 1024,
                    'swap_kib': latest['swap_kib'], 'threads': latest['threads'], 'fds': latest['fds'],
                }), flush=True)
                last_heartbeat = now
            return readings[-1] if readings else None

        def cli(*arguments):
            before = time.monotonic()
            result = subprocess.run([str(client), '--socket', str(endpoint), *map(str, arguments)],
                                    capture_output=True, text=True, timeout=10)
            events.append({
                'elapsed_seconds': before - started, 'phase': phase, 'cycle': cycle,
                'arguments': list(map(str, arguments)),
                'acceptance_ms': (time.monotonic() - before) * 1000,
                'exit_code': result.returncode,
            })
            if result.returncode:
                raise RuntimeError('CLI failed: ' + result.stderr)
            return json.loads(result.stdout)

        def state(expected):
            current = cli('state')
            if current != {'state': expected, 'error': None}:
                raise RuntimeError('unexpected state: ' + json.dumps(current))

        def hold(seconds):
            deadline = time.monotonic() + seconds * scale
            while (remaining := deadline - time.monotonic()) > 0:
                tick()
                time.sleep(min(1, remaining))
            tick(force=True)

        def play(name, path, region=()):
            nonlocal asset
            asset = name
            cli('display', 'video', path, *region)
            state('playing')
            tick()

        def clear(kind):
            nonlocal asset
            asset = 'blank'
            cli('display', 'clear')
            state('blank')
            row = tick(force=True).copy()
            row['kind'] = kind
            clear_points.append(row)

        failure = None
        try:
            deadline = time.monotonic() + 10
            while not endpoint.exists():
                if process.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError('daemon startup failed: ' + log.read_text())
                time.sleep(0.02)
            state('blank')
            time.sleep(1)
            first = tick(force=True)
            baseline_threads, baseline_fds = first['threads'], first['fds']
            phase = 'baseline'
            clear('baseline')
            hold(30)
            phase = 'warmup'
            play('fry-full', fry)
            hold(3)
            phase = 'continuous'
            hold(600)
            state('playing')
            phase = 'switching'
            deadline = time.monotonic() + 900 * scale
            while time.monotonic() < deadline:
                cycle += 1
                for name, path, region, seconds in [
                    ('fry-full', fry, (), 3), ('extra-full', extra, (), 3),
                    ('near-cap-62', near, (), 4),
                    ('fry-region', fry, ('--columns', '56..184', '--rows', '12..140'), 3),
                    ('over-cap-68', fallback, (), 4),
                ]:
                    play(name, path, region)
                    hold(seconds)
                clear('switching')
                hold(1)
            phase = 'rapid'
            deadline = time.monotonic() + 180 * scale
            while time.monotonic() < deadline:
                cycle += 1
                for name, path in [('near-cap-62', near), ('extra-full', extra),
                                   ('over-cap-68', fallback), ('fry-full', fry)]:
                    play(name, path)
                    hold(0.05)
                clear('rapid')
                hold(0.05)
            phase = 'cleared-idle'
            clear('final')
            hold(120)
            state('blank')
        except BaseException as error:
            failure = repr(error)
        finally:
            if process.poll() is None:
                process.send_signal(signal.SIGTERM)
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
                failure = failure or 'shutdown timed out'
            if process.returncode or log.read_text() or endpoint.exists():
                failure = failure or 'unclean shutdown'
            save('soak-samples.json', readings)
            save('soak-events.json', events)
            save('soak-clears.json', clear_points)

    phases, growth = summarize(readings, clear_points)
    if not args.quick:
        for name, value in growth.items():
            limit = 1024 if name == 'continuous_kib' else GROWTH_LIMIT_KIB
            if value > limit:
                failure = failure or name + ' exceeded the growth limit'
        if len(growth) != 3:
            failure = failure or 'not enough samples for all memory-growth checks'
    summary = {
        'daemon_pid': process.pid, 'shutdown_exit_code': process.returncode,
        'result': 'fail' if failure else 'pass', 'failure': failure,
        'quick': args.quick, 'growth_checks_enforced': not args.quick,
        'elapsed_seconds': time.monotonic() - started,
        'phase_summary': phases, 'growth': growth,
        'display_submissions': sum(event['arguments'][:2] == ['display', 'video'] for event in events),
        'clear_submissions': sum(event['arguments'][:2] == ['display', 'clear'] for event in events),
        'max_rss_kib': max((row['rss_kib'] for row in readings), default=0),
        'kernel_peak_rss_kib': max((row['peak_rss_kib'] for row in readings), default=0),
        'max_swap_kib': max((row['swap_kib'] for row in readings), default=0),
        'threads': sorted({row['threads'] for row in readings}),
        'fds_range': [min((row['fds'] for row in readings), default=0),
                      max((row['fds'] for row in readings), default=0)],
        'scope': 'One daemon throughout; original frame delays; one-second sampling plus clear '
                 'checkpoints. RSS may retain freed allocator storage. Quick mode is a smoke '
                 'check only; the full run checks finite memory trends, not unlimited-duration behavior.',
    }
    save('soak-summary.json', summary)
    print(json.dumps(summary), flush=True)
    if failure:
        raise SystemExit(1)


if __name__ == '__main__':
    main()
