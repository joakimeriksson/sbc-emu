#!/usr/bin/env python3
"""A shared library for aarch64 Linux whose functions do nothing and return 0, in place of one that another
library names but, for the purpose, does not need.

    stub_library.py USER VERSION-PREFIX SONAME OUT
        OUT gets every symbol that the shared library USER wants in a version that starts with
        VERSION-PREFIX
    stub_library.py --closure DIR EXTRA LIB[=FILE]...
        the libraries in DIR that LIB... need, with theirs (those in EXTRA count as there): lines of
        name and file
"""
import os
import struct
import sys


def u16(d, o):
    return struct.unpack_from('<H', d, o)[0]


def u32(d, o):
    return struct.unpack_from('<I', d, o)[0]


def u64(d, o):
    return struct.unpack_from('<Q', d, o)[0]


def text(d, o):
    return d[o:d.index(b'\0', o)].decode()


class Elf:
    def __init__(self, path):
        with open(path, 'rb') as f:
            d = self.d = f.read()
        assert d[:6] == b'\x7fELF\x02\x01' and u16(d, 18) == 183, path + ': not aarch64 ELF'
        self.sections = []
        for k in range(u16(d, 0x3c)):
            o = u64(d, 0x28) + 64 * k
            self.sections.append(dict(type=u32(d, o + 4), off=u64(d, o + 24), size=u64(d, o + 32), link=u32(d, o + 40), info=u32(d, o + 44)))

    def section(self, kind):
        return next((s for s in self.sections if s['type'] == kind), None)

    def needed(self):
        s = self.section(6)  # dynamic
        if s is None:
            return []
        names = self.sections[s['link']]['off']
        return [text(self.d, names + u64(self.d, o + 8)) for o in range(s['off'], s['off'] + s['size'], 16) if u64(self.d, o) == 1]

    def wanted(self, prefix):
        """Undefined symbols in versions with a prefix: (name, version)."""
        d, sym, ver, need = self.d, self.section(11), self.section(0x6fffffff), self.section(0x6ffffffe)
        names = self.sections[sym['link']]['off']
        versions, o = {}, need['off']
        for _ in range(need['info']):
            a = o + u32(d, o + 8)
            for _ in range(u16(d, o + 2)):
                versions[u16(d, a + 6)] = text(d, self.sections[need['link']]['off'] + u32(d, a + 8))
                a += u32(d, a + 12)
            o += u32(d, o + 12)
        out = []
        for k in range(sym['size'] // 24):
            o = sym['off'] + 24 * k
            v = versions.get(u16(d, ver['off'] + 2 * k) & 0x7fff, '')
            if u16(d, o + 6) == 0 and v.startswith(prefix):
                out.append((text(d, names + u32(d, o)), v))
        return out


def elf_hash(name):
    h = 0
    for c in name.encode():
        h = (h << 4) + c
        g = h & 0xf0000000
        h = (h ^ g >> 24) & ~g & 0xffffffff
    return h


def stub(symbols, soname, version):
    strings = bytearray(b'\0')

    def string(s):
        strings.extend(s.encode() + b'\0')
        return len(strings) - len(s) - 1

    names = [string(n) for n, _ in symbols]
    so, vname = string(soname), string(version)
    page = 0x10000
    n = len(symbols) + 1
    # one segment to read and run: the headers, the tables, the code
    at = 64 + 56 * 3
    dynsym = at + -at % 8
    hashes = dynsym + 24 * n
    versym = hashes + 4 * (2 + n + n)
    at = versym + 2 * n
    verdef = at + -at % 8
    dynstr = verdef + 2 * 28
    at = dynstr + len(strings)
    code = at + -at % 16
    # "mov x0, #0; mov x1, #0; ret" for each: they have their own addresses
    body = struct.pack('<4I', 0xd2800000, 0xd2800001, 0xd65f03c0, 0xd503201f) * len(symbols)
    end = code + len(body)
    dyn = end + -end % page
    out = bytearray(dyn)
    out[code:end] = body
    out[dynstr:dynstr + len(strings)] = strings
    for k, name in enumerate(names):
        struct.pack_into('<IBBHQQ', out, dynsym + 24 * (k + 1), name, 0x12, 0, 1, code + 16 * k, 16)
        struct.pack_into('<H', out, versym + 2 * (k + 1), 2)
    # a hash table with as many buckets as symbols
    heads, chain = [0] * n, [0] * n
    for k, (name, _) in enumerate(symbols):
        b = elf_hash(name) % n
        chain[k + 1], heads[b] = heads[b], k + 1
    struct.pack_into('<%dI' % (2 + 2 * n), out, hashes, n, n, *heads, *chain)
    struct.pack_into('<HHHHIII', out, verdef, 1, 1, 1, 1, elf_hash(soname), 20, 28)
    struct.pack_into('<II', out, verdef + 20, so, 0)
    struct.pack_into('<HHHHIII', out, verdef + 28, 1, 0, 2, 1, elf_hash(version), 20, 0)
    struct.pack_into('<II', out, verdef + 48, vname, 0)
    dynamic = b''.join(struct.pack('<QQ', t, v) for t, v in [
        (14, so), (4, hashes), (5, dynstr), (6, dynsym), (10, len(strings)), (11, 24),
        (0x6ffffff0, versym), (0x6ffffffc, verdef), (0x6ffffffd, 2), (0, 0)])
    struct.pack_into('<16sHHIQQQIHHHHHH', out, 0, b'\x7fELF\x02\x01\x01' + bytes(9), 3, 183, 1, 0, 64, 0, 0, 64, 56, 3, 0, 0, 0)
    heads = [(1, 5, 0, end, page), (1, 6, dyn, len(dynamic), page), (2, 6, dyn, len(dynamic), 8)]
    for k, (t, flags, at, size, align) in enumerate(heads):
        struct.pack_into('<IIQQQQQQ', out, 64 + 56 * k, t, flags, at, at, at, size, size, align)
    return bytes(out) + dynamic


def closure(dir_, extra, libs):
    todo, seen = [], {}
    for lib in libs:
        name, _, file = lib.partition('=')
        todo.append((name, os.path.join(dir_, file or name)))
    while todo:
        name, path = todo.pop()
        if name in seen:
            continue
        seen[name] = path
        for n in Elf(path).needed():
            f = next((p for p in (os.path.join(extra, n), os.path.join(dir_, n)) if os.path.exists(p)), None)
            if f is None:
                sys.exit('%s: needs %s, which is in none of the packages' % (name, n))
            todo.append((n, f))
    for name, path in sorted(seen.items()):
        if not path.startswith(extra):
            print('gbm/dri_gbm.so' if name == 'dri_gbm.so' else name, os.path.realpath(path))


def main():
    if sys.argv[1] == '--closure':
        closure(sys.argv[2], sys.argv[3], sys.argv[4:])
        return
    user, prefix, soname, path = sys.argv[1:]
    symbols = Elf(user).wanted(prefix)
    versions = {v for _, v in symbols}
    assert len(versions) == 1, versions
    with open(path, 'wb') as f:
        f.write(stub(symbols, soname, versions.pop()))
    print('%s: %d functions that do nothing' % (soname, len(symbols)))


main()
