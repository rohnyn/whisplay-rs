"""Local GIF-region acceptance latency. Provision assets before running."""
import hashlib
import json
import os
from pathlib import Path
import signal
import statistics
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
PROFILE = os.environ.get('WHISPLAY_PROFILE', 'release')
DAEMON = ROOT / 'target' / PROFILE / 'whisplayd'
CLI = ROOT / 'target' / PROFILE / 'whisplay'
ASSETS = Path(os.environ.get('WHISPLAY_EMOJI_DIR', str(Path.home() / '.local/share/whisplay/emoji')))
ASSET = ASSETS / '1f440.gif'
if not ASSET.is_file():
    raise SystemExit('Install a local 1f440.gif in WHISPLAY_EMOJI_DIR before this test; no assets are fetched.')

# Catch both whole-animation transcoding and eager full-font preparation.
FIRST_STARTUP_MS = 200
WARM_STARTUP_MS = 150
with tempfile.TemporaryDirectory(prefix='whisplay-region-') as work:
    endpoint = Path(work) / 'display.sock'
    args = [str(DAEMON), '--socket', str(endpoint)]
    hardware = os.environ.get('WHISPLAY_REAL_HARDWARE') == '1'
    if not hardware:
        args += ['--simulate']
    process = subprocess.Popen(args, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    env = {**os.environ, 'WHISPLAY_EMOJI_DIR': str(ASSETS)}

    def cli(*args):
        result = subprocess.run([str(CLI), '--socket', str(endpoint), *args], env=env, capture_output=True, text=True, timeout=5)
        assert result.returncode == 0, (args, result.stdout, result.stderr)
        return json.loads(result.stdout)

    try:
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise AssertionError(process.stderr.read())
            if endpoint.exists():
                try:
                    cli('display', 'clear')
                    break
                except AssertionError:
                    pass
            time.sleep(0.02)
        samples = []
        for index in range(6):
            start = time.monotonic()
            cli('display', 'text-and-emoji', 'Hello Pi', '👀')
            submit_ms = (time.monotonic() - start) * 1000
            samples.append({'submit_ms': round(submit_ms, 2)})
            budget = FIRST_STARTUP_MS if index == 0 else WARM_STARTUP_MS
            assert submit_ms < budget, samples
            if index == 0:
                time.sleep(1)
                assert process.poll() is None  # The CLI exits; the daemon remains independent.
            cli('display', 'clear')
        asset = ASSET.read_bytes()
        print(json.dumps({'result': 'pass', 'hardware': hardware, 'first_call_budget_ms': FIRST_STARTUP_MS, 'warm_budget_ms': WARM_STARTUP_MS, 'samples': samples, 'median_submit_ms': round(statistics.median(sample['submit_ms'] for sample in samples), 2), 'asset_bytes': len(asset), 'asset_sha256': hashlib.sha256(asset).hexdigest(), 'measurement': 'Acceptance only; LCD transfer and scanout completion are not measured.'}))
    finally:
        if process.poll() is None:
            process.send_signal(signal.SIGTERM)
        stdout, stderr = process.communicate(timeout=5)
        assert process.returncode == 0, (process.returncode, stdout, stderr)
        assert 'display:' not in stderr, stderr
        assert not endpoint.exists()
