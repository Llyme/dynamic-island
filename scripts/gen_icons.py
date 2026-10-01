"""Generates the app/tray icons (dark rounded square, two green island eyes)
as raw PNGs, no Pillow dependency -- just zlib + struct. Re-run to rebuild
src-tauri/icons/."""
import struct
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ICON_DIR = ROOT / "src-tauri" / "icons"


BG = (14, 14, 16)
EYE = (90, 200, 140)
SS = 3  # supersamples per axis, for smooth edges


def _in_rrect(x, y, cx, cy, hw, hh, r):
    """Point inside a rounded rect centered (cx, cy), half extents hw/hh, corner radius r."""
    dx = max(abs(x - cx) - (hw - r), 0)
    dy = max(abs(y - cy) - (hh - r), 0)
    return dx * dx + dy * dy <= r * r


def _rounded_square_rgba(size):
    """Dark rounded square with the island's two vertical green eyes."""
    eye_hw, eye_hh = size * 0.07, size * 0.15
    eyes = [(size * 0.34, size * 0.5), (size * 0.66, size * 0.5)]
    rows = []
    for y in range(size):
        row = bytearray()
        for x in range(size):
            acc = [0, 0, 0, 0]
            for sy in range(SS):
                for sx in range(SS):
                    px = x + (sx + 0.5) / SS
                    py = y + (sy + 0.5) / SS
                    if not _in_rrect(px, py, size / 2, size / 2, size / 2, size / 2, size * 0.28):
                        continue
                    on_eye = any(
                        _in_rrect(px, py, ex, ey, eye_hw, eye_hh, eye_hw) for ex, ey in eyes
                    )
                    r, g, b = EYE if on_eye else BG
                    acc[0] += r
                    acc[1] += g
                    acc[2] += b
                    acc[3] += 255
            n = SS * SS
            cov = acc[3] // 255
            if cov == 0:
                row.extend((0, 0, 0, 0))
            else:
                row.extend((acc[0] // cov, acc[1] // cov, acc[2] // cov, acc[3] // n))
        rows.append(bytes(row))
    return rows


def write_png(path, size):
    rows = _rounded_square_rgba(size)
    raw = b"".join(b"\x00" + row for row in rows)
    compressed = zlib.compress(raw, 9)

    def chunk(tag, data):
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data))
        )

    sig = b"\x89PNG\r\n\x1a\n"
    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    png = sig + chunk(b"IHDR", ihdr) + chunk(b"IDAT", compressed) + chunk(b"IEND", b"")
    path.write_bytes(png)


def write_ico(path, sizes):
    # ICO container wrapping PNG-compressed entries (valid since Vista)
    entries = []
    offset = 6 + 16 * len(sizes)
    images = []
    for size in sizes:
        rows = _rounded_square_rgba(size)
        raw = b"".join(b"\x00" + row for row in rows)
        compressed = zlib.compress(raw, 9)

        def chunk(tag, data):
            return (
                struct.pack(">I", len(data))
                + tag
                + data
                + struct.pack(">I", zlib.crc32(tag + data))
            )

        sig = b"\x89PNG\r\n\x1a\n"
        ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
        png = sig + chunk(b"IHDR", ihdr) + chunk(b"IDAT", compressed) + chunk(b"IEND", b"")
        images.append(png)

    for size, img in zip(sizes, images):
        w = 0 if size >= 256 else size
        h = 0 if size >= 256 else size
        entries.append(
            struct.pack("<BBBBHHII", w, h, 0, 0, 1, 32, len(img), offset)
        )
        offset += len(img)

    header = struct.pack("<HHH", 0, 1, len(sizes))
    path.write_bytes(header + b"".join(entries) + b"".join(images))


if __name__ == "__main__":
    ICON_DIR.mkdir(parents=True, exist_ok=True)
    for size, name in ((32, "32x32.png"), (128, "128x128.png"), (256, "128x128@2x.png")):
        write_png(ICON_DIR / name, size)
    write_ico(ICON_DIR / "icon.ico", [16, 32, 48, 256])
    print("icons written to", ICON_DIR)
