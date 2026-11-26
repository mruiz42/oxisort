#!/usr/bin/env python3
"""Writes tests/fixtures/sr2_far_data.arw: a synthetic, image-free
little-endian TIFF/ARW shaped like a Sony A7C II (ILCE-7CM2) header.

Why: nom-exif 3.8.0 fails the whole parse ("parse ifd entry header failed:
Incomplete(...)") when any IFD0 entry's data ends past the bytes it has
buffered, instead of reading more. The A7C II puts such an entry in IFD0.
Here that entry is a 60,000-byte XMP block (zeros) at offset 2048, so the
date in ExifIFD is unreachable for nom-exif. An SR2Private pointer and SR2
directory are included for shape; no real photo data is involved.

Usage: python3 scripts/gen-sr2-arw.py [OUT_PATH]
"""

import struct
import sys

BYTE, ASCII, LONG = 1, 2, 4
XMP_AT, XMP_LEN = 2048, 60_000


def ifd(at, entries):
    """Packs an IFD placed at offset `at`, followed by its out-of-line data.
    Entries are (tag, type, count, value): an int value is stored as is (an
    inline number or an offset), bytes of up to 4 go inline, longer bytes
    right after the IFD."""
    out = struct.pack("<H", len(entries))
    data_at = at + 2 + 12 * len(entries) + 4
    tail = b""
    for tag, typ, count, value in sorted(entries):
        if isinstance(value, bytes) and len(value) > 4:
            off = data_at + len(tail)
            tail += value + (b"\0" if len(value) % 2 else b"")
            out += struct.pack("<HHII", tag, typ, count, off)
        elif isinstance(value, bytes):
            out += struct.pack("<HHI", tag, typ, count) + value.ljust(4, b"\0")
        else:
            out += struct.pack("<HHII", tag, typ, count, value)
    out += struct.pack("<I", 0)
    return out + tail


def asc(s):
    b = s.encode() + b"\0"
    return (ASCII, len(b), b)


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "tests/fixtures/sr2_far_data.arw"
    exif_at, sr2_at = 512, 1024
    ifd0 = ifd(8, [
        (0x010F, *asc("SONY")),
        (0x0110, *asc("ILCE-7CM2")),
        (0x0132, *asc("2026:01:02 06:11:40")),
        (0x02BC, BYTE, XMP_LEN, XMP_AT),
        (0x8769, LONG, 1, exif_at),
        (0xC634, LONG, 1, sr2_at),
    ])
    exif = ifd(exif_at, [
        (0x9003, *asc("2026:01:02 06:11:39")),
        (0x9004, *asc("2026:01:02 06:11:39")),
        (0x9011, *asc("-07:00")),
        (0x9291, *asc("705")),
    ])
    sr2 = ifd(sr2_at, [(0x7200, LONG, 1, 65826), (0x7201, LONG, 1, 35526)])
    buf = bytearray(XMP_AT + XMP_LEN)
    buf[0:8] = b"II*\0" + struct.pack("<I", 8)
    # A minimal valid XMP packet, space-padded to the declared length.
    head = b'<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?>'
    head += b'<x:xmpmeta xmlns:x="adobe:ns:meta/"/>'
    tail = b'<?xpacket end="w"?>'
    buf[XMP_AT:] = head + b" " * (XMP_LEN - len(head) - len(tail)) + tail
    for at, blob in ((8, ifd0), (exif_at, exif), (sr2_at, sr2)):
        assert at + len(blob) <= {8: exif_at, exif_at: sr2_at, sr2_at: XMP_AT}[at]
        buf[at:at + len(blob)] = blob
    with open(path, "wb") as f:
        f.write(buf)


if __name__ == "__main__":
    main()
