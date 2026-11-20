"""Exercise the real CLI/socket boundary; only hardware access is simulated."""
import json
import os
from pathlib import Path
import signal
import socket
import struct
import subprocess
import tempfile
import time
import zlib

ROOT = Path(__file__).resolve().parents[1]
PROFILE = os.environ.get('WHISPLAY_PROFILE', 'debug')
DAEMON = ROOT / 'target' / PROFILE / 'whisplayd'
CLI = ROOT / 'target' / PROFILE / 'whisplay'


def png(path):
    def chunk(kind, data):
        return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data))
    path.write_bytes(b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', 1, 1, 8, 6, 0, 0, 0)) + chunk(b'IDAT', zlib.compress(b'\0\xff\0\0\xff')) + chunk(b'IEND', b''))


with tempfile.TemporaryDirectory(prefix='whisplay-smoke-') as work:
    work = Path(work)
    endpoint = work / 'display.sock'
    args = [str(DAEMON), '--socket', str(endpoint)]
    if os.environ.get('WHISPLAY_REAL_HARDWARE') != '1':
        args += ['--simulate']
    process = subprocess.Popen(args, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    env = {**os.environ, 'WHISPLAY_EMOJI_DIR': str(work / 'emoji')}

    def cli(*args, ok=True):
        result = subprocess.run([str(CLI), '--socket', str(endpoint), *args], env=env, capture_output=True, text=True, timeout=60)
        assert (result.returncode == 0) == ok, (args, result.stdout, result.stderr)
        return json.loads(result.stdout) if ok else result.stderr

    def ready():
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise AssertionError(process.stderr.read())
            if endpoint.exists():
                try:
                    return cli('display', 'clear')
                except AssertionError:
                    pass
            time.sleep(0.02)
        raise AssertionError('daemon did not become ready')

    try:
        assert ready() == {'ok': True}
        image = work / 'red.png'
        png(image)
        cli('display', 'image', '--file', str(image))
        image.unlink()  # The daemon must own bytes after acknowledging.
        invalid = work / 'invalid.png'
        invalid.write_text('not an image')
        cli('display', 'image', '--file', str(invalid), ok=False)
        oversized = work / 'oversized.png'
        with oversized.open('wb') as source:
            source.truncate(16 * 1024 * 1024 + 1)
        cli('display', 'image', '--file', str(oversized), ok=False)
        pixels = work / 'patch.rgb565'
        pixels.write_bytes(bytes([0x07, 0xe0]))
        cli('display', 'image', '--raw-pixels', str(pixels), '--columns', '239..240', '--rows', '279..280')
        pixels.write_bytes(b'bad')
        cli('display', 'image', '--raw-pixels', str(pixels), '--columns', '239..240', '--rows', '279..280', ok=False)
        cli('display', 'image', '--raw-pixels', str(pixels), '--columns', '10..5', ok=False)
        cli('display', 'brightness', '101', ok=False)
        cli('display', 'brightness', '50')
        time.sleep(0.1)
        cli('display', 'brightness', '100')
        with socket.socket(socket.AF_UNIX) as connection:
            connection.connect(str(endpoint))
            connection.sendall(b'{"command":')
            time.sleep(0.02)
            connection.sendall(b'"backlight","percent":100}\n')
            assert json.loads(connection.makefile().readline())['result'] == 'ok'
        with socket.socket(socket.AF_UNIX) as connection:
            connection.connect(str(endpoint))
            connection.sendall(b'{"command":"unknown"}\n')
            assert json.loads(connection.makefile().readline())['result'] == 'error'
        cli('display', 'text', 'Hello Pi')
        cli('display', 'text', '--', '--help')  # A literal text value, not a CLI flag.
        cli('display', 'text', 'word ' * 300, ok=False)
        cli('display', 'emoji', '👀', ok=False)  # Missing local file; never fetch.
        assets = work / 'emoji'
        assets.mkdir()
        png(assets / '1f440.png')
        cli('display', 'text-and-emoji', 'Local PNG', '👀')
        (assets / '1f440.png').unlink()
        (assets / '1f440.gif').write_text('broken GIF')
        cli('display', 'text-and-emoji', 'Caption stays', '👀', ok=False)
        cli('display', 'clear')
    finally:
        if process.poll() is None:
            process.send_signal(signal.SIGTERM)
        stdout, stderr = process.communicate(timeout=5)
        assert process.returncode == 0, (process.returncode, stdout, stderr)
        assert 'display:' not in stderr, stderr
        assert not endpoint.exists(), 'socket survived clean shutdown'
    print('PASS: CLI, file ownership, pixel ranges, malformed/fragmented requests, text, local emoji, partial failure and shutdown')
