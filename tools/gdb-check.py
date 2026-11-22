#!/usr/bin/env python3
"""Check the gdb stub on a board with a booted guest, over the remote serial protocol.

  tools/gdb-check.py [BOARD ...]      (default: every board whose guest is in cache/)

Boots the board's direct-boot guest to its shell, then, as a debugger would: reads the register
description, threads, registers and memory; writes a register and memory and puts them back;
single-steps; stops at a breakpoint and at a write watchpoint; detaches; and checks that the
guest's shell still works. Then a second run waits for the debugger before its first instruction
(--gdb-wait). CACHE=DIR picks another cache, S31=DIR the ESP32-S31's (rom/, out/), NANO=DIR the Jetson Nano's.
Exit status 0 only if everything passed on every board run.
"""
import os, re, select, socket, struct, subprocess, sys, time
import xml.etree.ElementTree as ET

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CACHE = os.environ.get('CACHE', os.path.join(ROOT, 'cache'))
S31 = os.environ.get('S31', os.path.join(CACHE, 'esp32s31'))
EMU = os.environ.get('SBC_EMU', os.path.join(ROOT, 'target/release/sbc-emu'))
c = lambda *p: os.path.join(CACHE, *p)
n = lambda *p: os.path.join(os.environ.get('NANO', c('jetson-nano')), *p)

# the guest, its shell prompt, its cores, its architecture
BOARDS = {
    'pi4b': (['--kernel', c('fw', 'Image'), '--initrd', c('initramfs.cpio'), '--shell', 'serial', '--ram-mb', '512', '--net', 'none'], 'pi4# ', 4, 'aarch64'),
    'orangepi5': (['--kernel', c('orangepi5', 'Image'), '--initrd', c('orangepi5', 'initramfs.cpio.gz'), '--smp', '4', '--ram-mb', '512', '--net', 'none'], 'orangepi5# ', 4, 'aarch64'),
    'orangepi-one': (['--kernel', c('orangepi-one', 'zImage'), '--dtb', c('orangepi-one', 'sun8i-h3-orangepi-one.dtb'), '--initrd', c('orangepi-one', 'initramfs.cpio.gz')], 'orangepi-one# ', 4, 'arm'),
    'tinkerboard': (['--kernel', c('tinker-board', 'zImage'), '--dtb', c('tinker-board', 'rk3288-tinker-s.dtb'), '--initrd', c('tinker-board', 'initramfs.cpio.gz')], 'tinker-board# ', 4, 'arm'),
    'orangepi-rv': (['--firmware', c('orangepi-rv', 'fw_dynamic.bin'), '--kernel', c('orangepi-rv', 'Image'), '--dtb', c('orangepi-rv', 'jh7110-orangepi-rv.dtb'), '--initrd', c('orangepi-rv', 'initramfs.cpio.gz')], 'orangepi-rv# ', 5, 'riscv:rv64'),
    'jetson-nano': (['--kernel', n('vmlinuz'), '--dtb', n('tegra210-p3450-0000.dtb'), '--initrd', n('initramfs.cpio.gz')], 'jetson-nano# ', 4, 'aarch64'),
    'esp32s31': (['--rom', os.path.join(S31, os.environ.get('S31_ROM', 'esp32s31_rev0_rom.elf')), '--flash', os.path.join(S31, os.environ.get('S31_FLASH', 'images/s31_full_flash.bin'))], '~ # ', 2, 'riscv:rv32'),
}


class Rsp:
    def __init__(self, port):
        end = time.time() + 10
        while True:
            try:
                self.s = socket.create_connection(('127.0.0.1', port), timeout=1)
                break
            except OSError:
                if time.time() > end: raise
                time.sleep(0.05)
        self.buf = b''

    def send(self, body):
        self.s.sendall(b'$%s#%02x' % (body.encode(), sum(body.encode()) & 0xff))

    def recv(self, timeout=60):
        end = time.time() + timeout
        while True:
            m = re.search(rb'\$([^#]*)#[0-9a-f]{2}', self.buf)
            if m:
                self.buf = self.buf[m.end():]
                self.s.sendall(b'+')
                return m.group(1).decode()
            left = end - time.time()
            if left <= 0: raise TimeoutError('no reply from the stub')
            if select.select([self.s], [], [], left)[0]:
                d = self.s.recv(65536)
                if not d: raise ConnectionError('the stub closed the connection')
                self.buf += d

    def cmd(self, body, timeout=60):
        self.send(body)
        return self.recv(timeout)


class Guest:
    def __init__(self, board, port, *extra):
        args, self.prompt, self.cores, self.arch = BOARDS[board]
        for a in args:
            if a.startswith('/') and not os.path.exists(a): raise FileNotFoundError(a)
        self.p = subprocess.Popen([EMU, 'run', '--board', board, *args, '--gdb', str(port), *extra],
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        os.set_blocking(self.p.stdout.fileno(), False)
        self.out = ''
        self.seen = 0

    def wait(self, text, timeout):
        end = time.time() + timeout
        while True:
            d = self.p.stdout.read()
            if d: self.out += d.decode('latin1')
            at = self.out.find(text, self.seen)
            if at >= 0:
                self.seen = at + len(text)
                return
            if self.p.poll() is not None: raise RuntimeError(f'the emulator exited: {self.out[-600:]}')
            if time.time() > end: raise TimeoutError(f'{text!r} did not appear; last output: {self.out[-400:]!r}')
            time.sleep(0.02)

    def type(self, text):
        self.p.stdin.write(text.encode())
        self.p.stdin.flush()

    def kill(self):
        self.p.kill()
        self.p.wait()


class Dbg:
    """Registers by name, from the target's own description."""
    def __init__(self, rsp):
        self.r = rsp
        xml, off = '', 0
        while True:
            part = rsp.cmd(f'qXfer:features:read:target.xml:{off:x},{0x800:x}')
            xml += part[1:]
            off += len(part) - 1
            if part[0] == 'l': break
        t = ET.fromstring(xml)
        self.arch = t.find('architecture').text
        self.regs = [(r.get('name'), int(r.get('bitsize')) // 8) for r in t.iter('reg')]
        self.num = {n: k for k, (n, _) in enumerate(self.regs)}
        self.word = dict(self.regs)['pc']

    def reg(self, name): return int.from_bytes(bytes.fromhex(self.r.cmd(f'p{self.num[name]:x}')), 'little')
    def set(self, name, v): return self.r.cmd(f'P{self.num[name]:x}={v.to_bytes(dict(self.regs)[name], "little").hex()}')
    def mem(self, a, n):
        r = self.r.cmd(f'm{a:x},{n:x}')
        return None if r.startswith('E') else bytes.fromhex(r)
    def poke(self, a, d): return self.r.cmd(f'M{a:x},{len(d):x}:{d.hex()}')
    def thread(self, t): return self.r.cmd(f'Hg{t:x}')


def stop(reply):
    m = re.match(r'T([0-9a-f]{2})(.*)', reply)
    assert m, f'not a stop reply: {reply!r}'
    f = dict(x.split(':', 1) for x in m.group(2).split(';') if ':' in x)
    return int(m.group(1), 16), int(f['thread'], 16), f


def check(board, port):
    done = []
    def ok(what): done.append(what)
    g = Guest(board, port)
    try:
        g.wait(g.prompt, 120)
        r = Rsp(port)
        assert 'qXfer:features:read+' in r.cmd('qSupported:xmlRegisters=i386')
        sig, me, _ = stop(r.cmd('?'))
        ok(f'attach (signal {sig})')
        d = Dbg(r)
        assert d.arch == g.arch, (d.arch, g.arch)
        threads = [int(x, 16) for x in r.cmd('qfThreadInfo')[1:].split(',')]
        assert threads == list(range(1, g.cores + 1)), threads
        names = [bytes.fromhex(r.cmd(f'qThreadExtraInfo,{t:x}')).decode() for t in threads]
        ok(f'{len(threads)} threads ({"; ".join(names)})')
        # 'g' is every register in order, and agrees with 'p'
        for t in threads:
            d.thread(t)
            allr = bytes.fromhex(r.cmd('g'))
            assert len(allr) == sum(n for _, n in d.regs), (len(allr), sum(n for _, n in d.regs))
            off = sum(n for _, n in d.regs[:d.num['pc']])
            assert int.from_bytes(allr[off:off + d.word], 'little') == d.reg('pc')
        ok(f'{len(d.regs)} registers')
        # until the core idles (the same place at two stops): some kernels still compute something
        # when the shell comes up, on a task's stack that nothing uses again
        last = None
        for _ in range(60):
            d.thread(me)
            here = (d.reg('pc'), d.reg('sp'))
            if here == last: break
            last = here
            r.send('vCont;c')
            time.sleep(0.5)
            r.s.sendall(b'\x03')
            stop(r.recv())
        else:
            raise TimeoutError('the core does not become idle')
        # a running core's code and stack
        d.thread(me)
        pc, sp = d.reg('pc'), d.reg('sp')
        code = d.mem(pc, 16)
        assert code and any(code), f'no code at pc {pc:#x}'
        assert d.mem(sp, 64) is not None, f'no stack at {sp:#x}'
        ok('memory at pc and sp')
        # write a register and memory, and put them back
        name = {'aarch64': 'x15', 'arm': 'r8'}.get(g.arch, 't6')
        old, new = d.reg(name), 0x12345678
        assert d.set(name, new) == 'OK' and d.reg(name) == new
        assert d.set(name, old) == 'OK' and d.reg(name) == old
        a = sp - 1024
        before = d.mem(a, 8)
        assert d.poke(a, b'\xa5\x5aDEBUG!') == 'OK' and d.mem(a, 8) == b'\xa5\x5aDEBUG!'
        assert d.poke(a, before) == 'OK' and d.mem(a, 8) == before
        ok('register and memory writes')
        # single steps: every one moves the pc or is a branch to itself
        pcs = []
        for _ in range(200):
            sig, t, _ = stop(r.cmd(f'vCont;s:{me:x}'))
            assert (sig, t) == (5, me), (sig, t)
            pcs.append(d.reg('pc'))
        assert len(set(pcs)) > 5, [hex(x) for x in pcs[:10]]
        ok(f'200 steps ({len(set(pcs))} pcs)')
        # a breakpoint: typing at the shell makes the guest run; stop where it has been
        target = max(set(pcs), key=pcs.count)
        hits = []
        assert r.cmd(f'Z0,{target:x},4') == 'OK'
        for _ in range(3):
            r.send('vCont;c')
            g.type('\r')
            sig, t, _ = stop(r.recv())
            d.thread(t)
            assert sig == 5 and d.reg('pc') == target, (sig, hex(d.reg('pc')), hex(target))
            hits.append(t)
        # stepping off a breakpoint executes it
        stop(r.cmd(f'vCont;s:{hits[-1]:x}'))
        assert r.cmd(f'z0,{target:x},4') == 'OK'
        ok(f'breakpoint at {target:#x} (threads {hits})')
        # a write watchpoint on the kernel stack below sp: the next interrupt saves registers there
        d.thread(me)
        sp = d.reg('sp')
        lo, n = sp - 256, 256
        assert r.cmd(f'Z2,{lo:x},{n:x}') == 'OK'
        r.send('vCont;c')
        g.type('\r')
        sig, t, f = stop(r.recv())
        assert sig == 5 and int(f.get('watch', '0'), 16) == lo, (sig, f, hex(lo))
        assert r.cmd(f'z2,{lo:x},{n:x}') == 'OK'
        ok(f'write watchpoint (thread {t})')
        # a read watchpoint on the word the interrupted code saved there last
        assert r.cmd(f'Z3,{lo:x},{n:x}') == 'OK'
        r.send('vCont;c')
        g.type('\r')
        sig, t, f = stop(r.recv())
        assert sig == 5 and int(f.get('rwatch', '0'), 16) == lo, (sig, f, hex(lo))
        assert r.cmd(f'z3,{lo:x},{n:x}') == 'OK'
        ok(f'read watchpoint (thread {t})')
        # nothing set: it runs, and Ctrl-C stops it
        r.send('vCont;c')
        time.sleep(0.5)
        r.s.sendall(b'\x03')
        sig, _, _ = stop(r.recv())
        assert sig == 2, sig
        ok('continue and Ctrl-C')
        # leave with a breakpoint set: the guest must not stop on it
        assert r.cmd(f'Z0,{target:x},4') == 'OK'
        assert r.cmd('D') == 'OK'
        r.s.close()
        g.seen = len(g.out)
        g.type('\recho DE""TACHED $((6*7))\r')
        g.wait('DETACHED 42', 60)
        ok('detach, shell works')
        # a debugger that vanishes while the guest runs leaves nothing set either
        r = Rsp(port)
        stop(r.cmd('?'))
        assert r.cmd(f'Z0,{target:x},4') == 'OK'
        r.send('vCont;c')
        time.sleep(0.2)
        r.s.close()
        time.sleep(0.2)
        r = Rsp(port)
        stop(r.cmd('?'))
        r.send('vCont;c')
        g.seen = len(g.out)
        g.type('\recho VA""NISHED $((6*7))\r')
        g.wait('VANISHED 42', 60)
        r.s.setblocking(False)
        try:
            late = r.s.recv(4096)
        except BlockingIOError:
            late = b''
        assert b'$T' not in r.buf + late, f'stopped on a breakpoint of a debugger that went away: {late!r}'
        ok('vanished debugger')
    finally:
        g.kill()
    # stopped before the first instruction
    g = Guest(board, port + 1, '--gdb-wait')
    try:
        r = Rsp(port + 1)
        _, me, _ = stop(r.cmd('?'))
        d = Dbg(r)
        pc = d.reg('pc')
        for _ in range(5): stop(r.cmd(f'vCont;s:{me:x}'))
        assert d.reg('pc') != pc
        g.p.stdout.read()
        ok(f'--gdb-wait at {pc:#x}')
        r.cmd('D')
        g.wait(g.prompt, 120)
        ok('boots after detaching')
    finally:
        g.kill()
    return done


def main():
    boards = sys.argv[1:] or list(BOARDS)
    failed = False
    for k, b in enumerate(boards):
        t = time.time()
        try:
            done = check(b, 23400 + (os.getpid() % 500) * 20 + 2 * k)
            print(f'PASS {b} ({time.time() - t:.0f} s): ' + ', '.join(done), flush=True)
        except FileNotFoundError as e:
            print(f'SKIP {b}: no {e}', flush=True)
            if sys.argv[1:]: failed = True
        except Exception as e:
            failed = True
            print(f'FAIL {b}: {type(e).__name__}: {e}', flush=True)
            if os.environ.get('TRACE'): raise
    sys.exit(failed)


main()
