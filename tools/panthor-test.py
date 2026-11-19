#!/usr/bin/env python3
"""Upload a guest Python test to Linux/Panthor and require its success marker."""
import argparse
import base64
import pathlib
import subprocess
import tempfile
import zlib

root = pathlib.Path(__file__).resolve().parent.parent
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--image', type=pathlib.Path, required=True)
parser.add_argument('--guest', type=pathlib.Path, required=True)
parser.add_argument('--success-marker', required=True)
parser.add_argument('--log', type=pathlib.Path, required=True)
parser.add_argument('--emulator', type=pathlib.Path, default=root/'target/release/sbc-emu')
parser.add_argument('--timeout', type=int, default=180)
args = parser.parse_args()
payload = base64.b64encode(zlib.compress(args.guest.read_bytes())).decode()
lines = ['wait-serial root@',
         'serial mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t tmpfs none /tmp; modprobe fixed; modprobe panthor; stty -echo\\n',
         'serial : > /tmp/panthor-test.b64\\n']
for i in range(0, len(payload), 500):
    lines.append('serial printf %s '+payload[i:i+500]+' >> /tmp/panthor-test.b64\\n')
# The computed completion marker cannot match the shell's echoed command.
lines += ['serial python3 -c \'import base64,zlib;exec(zlib.decompress(base64.b64decode(open("/tmp/panthor-test.b64").read())))\'; result=$?; echo PANTHOR-EXIT-$result; echo PANTHOR-DONE-$((6*7))\\n',
          'wait-serial PANTHOR-DONE-42', 'quit']
with tempfile.TemporaryDirectory(prefix='panthor-test-') as directory:
    script = pathlib.Path(directory)/'script.txt'
    script.write_text('\n'.join(lines)+'\n')
    command = [str(args.emulator.resolve()), 'run', '--board', 'orangepi5', '--gpu', 'probe',
               '--net', 'none', '--sd', str(args.image.resolve()), '--append', 'init=/bin/bash',
               '--script', str(script), '--timeout', str(args.timeout)]
    try:
        result = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                timeout=args.timeout+30)
        stdout, stderr, code = result.stdout, result.stderr, result.returncode
    except subprocess.TimeoutExpired as error:
        stdout, stderr, code = error.stdout or b'', error.stderr or b'', 124
    log = stdout.decode(errors='replace')
    args.log.parent.mkdir(parents=True, exist_ok=True)
    args.log.write_text(log+'\n[host diagnostics]\n'+stderr.decode(errors='replace'))
    if code or args.success_marker not in log or 'PANTHOR-EXIT-0' not in log or 'PANTHOR-DONE-42' not in log:
        raise SystemExit('Panthor guest test failed; see '+str(args.log))
    print(args.success_marker+'; log: '+str(args.log))
