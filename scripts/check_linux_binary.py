#!/usr/bin/env python3
"""Check that a Linux release of rune starts on the Linux it is meant for.

    python3 -I scripts/check_linux_binary.py <rune> --libc gnu|musl

gnu:  every glibc symbol version the program asks for is at most GLIBC_2.17
      (the floor scripts/build_linux_release.sh builds for, and the oldest
      glibc Rust supports). One newer version and the program refuses to
      start on every system older than it — the fault this check exists for:
      a release built on Debian 13 asked for GLIBC_2.38 and 2.39 and would not
      start on Debian 12 (2026-10-21).
musl: the program needs no loader and no shared library at all.

Reads the ELF file itself (program headers, dynamic section, version needs):
no objdump/readelf needed, so it runs the same on a developer's machine and
in CI. Prints what it found; exits 1 when the binary does not qualify.
"""

import argparse
import struct
import sys

FLOOR = (2, 17)

PT_INTERP = 3
PT_DYNAMIC = 2
DT_NULL, DT_NEEDED, DT_STRTAB = 0, 1, 5
SHT_GNU_VERNEED = 0x6FFFFFFE


class Elf:
    def __init__(self, data):
        if data[:4] != b"\x7fELF":
            raise ValueError("not an ELF file")
        if data[4] != 2:
            raise ValueError("not a 64-bit ELF file")
        self.data = data
        self.end = "<" if data[5] == 1 else ">"
        (self.phoff, self.shoff) = self.unpack("QQ", 0x20)
        (self.phentsize, self.phnum, self.shentsize, self.shnum, self.shstrndx) = self.unpack(
            "HHHHH", 0x36
        )

    def unpack(self, fmt, offset):
        return struct.unpack_from(self.end + fmt, self.data, offset)

    def segments(self):
        for i in range(self.phnum):
            off = self.phoff + i * self.phentsize
            p_type, _flags, p_offset, p_vaddr, _paddr, p_filesz = self.unpack("IIQQQQ", off)
            yield p_type, p_offset, p_vaddr, p_filesz

    def sections(self):
        for i in range(self.shnum):
            off = self.shoff + i * self.shentsize
            _name, sh_type, _flags, _addr, sh_offset, sh_size, sh_link = self.unpack(
                "IIQQQQI", off
            )
            yield sh_type, sh_offset, sh_size, sh_link

    def section(self, index):
        return list(self.sections())[index]

    def cstr(self, offset):
        return self.data[offset : self.data.index(b"\0", offset)].decode()

    def interpreter(self):
        for p_type, p_offset, _vaddr, p_filesz in self.segments():
            if p_type == PT_INTERP:
                return self.data[p_offset : p_offset + p_filesz].rstrip(b"\0").decode()
        return None

    def needed(self):
        """Shared libraries named by DT_NEEDED, read through the dynamic
        section's string table (an address, mapped back to a file offset)."""
        dynamic = [s for s in self.segments() if s[0] == PT_DYNAMIC]
        if not dynamic:
            return []
        _t, offset, _v, size = dynamic[0]
        entries = []
        for at in range(offset, offset + size, 16):
            tag, value = self.unpack("qQ", at)
            if tag == DT_NULL:
                break
            entries.append((tag, value))
        strtab = next((v for t, v in entries if t == DT_STRTAB), None)
        if strtab is None:
            return []
        base = self.file_offset(strtab)
        return [self.cstr(base + v) for t, v in entries if t == DT_NEEDED]

    def file_offset(self, address):
        for _t, p_offset, p_vaddr, p_filesz in self.segments():
            if p_vaddr <= address < p_vaddr + p_filesz:
                return address - p_vaddr + p_offset
        raise ValueError(f"address {address:#x} is in no segment")

    def version_needs(self):
        """(library, version) for every version the program needs."""
        found = []
        for sh_type, sh_offset, _size, sh_link in self.sections():
            if sh_type != SHT_GNU_VERNEED:
                continue
            strings = self.section(sh_link)[1]
            at = sh_offset
            while True:
                _version, cnt, file_name, aux, nxt = self.unpack("HHIII", at)
                library = self.cstr(strings + file_name)
                aux_at = at + aux
                for _ in range(cnt):
                    _hash, _flags, _other, name, aux_next = self.unpack("IHHII", aux_at)
                    found.append((library, self.cstr(strings + name)))
                    if not aux_next:
                        break
                    aux_at += aux_next
                if not nxt:
                    break
                at += nxt
        return found


def glibc_version(name):
    if not name.startswith("GLIBC_"):
        return None
    try:
        return tuple(int(p) for p in name[len("GLIBC_") :].split("."))
    except ValueError:
        return None


def check(path, libc):
    with open(path, "rb") as f:
        elf = Elf(f.read())
    problems = []
    interpreter = elf.interpreter()
    needed = elf.needed()
    if libc == "musl":
        if interpreter or needed:
            problems.append(
                f"not static: loader {interpreter or 'none'}, libraries {needed or 'none'}"
            )
        print(f"{path}: static, no loader, no shared library")
    else:
        versions = sorted(
            {v for _lib, name in elf.version_needs() if (v := glibc_version(name))}
        )
        newest = versions[-1] if versions else None
        shown = ".".join(map(str, newest)) if newest else "none"
        print(f"{path}: loader {interpreter}; libraries {', '.join(needed)}; newest GLIBC_{shown}")
        too_new = [v for v in versions if v > FLOOR]
        if too_new:
            problems.append(
                "asks for "
                + ", ".join("GLIBC_" + ".".join(map(str, v)) for v in too_new)
                + f", newer than the floor GLIBC_{'.'.join(map(str, FLOOR))}"
            )
    for problem in problems:
        print(f"FAIL: {problem}", file=sys.stderr)
    return not problems


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("binary")
    parser.add_argument("--libc", choices=["gnu", "musl"], required=True)
    args = parser.parse_args()
    sys.exit(0 if check(args.binary, args.libc) else 1)


if __name__ == "__main__":
    main()
