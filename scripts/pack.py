from pathlib import Path
import struct
import zlib


def paeth(a: int, b: int, c: int) -> int:
    p = a + b - c
    pa = abs(p - a)
    pb = abs(p - b)
    pc = abs(p - c)
    if pa <= pb and pa <= pc:
        return a
    if pb <= pc:
        return b
    return c


def decode_png(path: Path):
    data = path.read_bytes()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise ValueError(f"{path} is not a PNG")
    pos = 8
    width = height = bit_depth = color_type = interlace = None
    idat = bytearray()
    while pos < len(data):
        length = struct.unpack_from(">I", data, pos)[0]
        kind = data[pos + 4:pos + 8]
        payload = data[pos + 8:pos + 8 + length]
        pos += 12 + length
        if kind == b"IHDR":
            width, height, bit_depth, color_type, _, _, interlace = struct.unpack(">IIBBBBB", payload)
        elif kind == b"IDAT":
            idat.extend(payload)
        elif kind == b"IEND":
            break
    if bit_depth != 8 or color_type not in (2, 6) or interlace != 0:
        raise ValueError(f"Unsupported PNG format for {path}: depth={bit_depth} color={color_type} interlace={interlace}")
    channels = 4 if color_type == 6 else 3
    stride = width * channels
    raw = zlib.decompress(bytes(idat))
    rows = []
    prev = bytearray(stride)
    offset = 0
    for _ in range(height):
        filter_type = raw[offset]
        offset += 1
        scan = bytearray(raw[offset:offset + stride])
        offset += stride
        recon = bytearray(stride)
        for i, value in enumerate(scan):
            a = recon[i - channels] if i >= channels else 0
            b = prev[i]
            c = prev[i - channels] if i >= channels else 0
            if filter_type == 0:
                predictor = 0
            elif filter_type == 1:
                predictor = a
            elif filter_type == 2:
                predictor = b
            elif filter_type == 3:
                predictor = (a + b) // 2
            elif filter_type == 4:
                predictor = paeth(a, b, c)
            else:
                raise ValueError(f"Unsupported PNG filter {filter_type} in {path}")
            recon[i] = (value + predictor) & 0xFF
        rows.append(recon)
        prev = recon
    return width, height, channels, rows


def png_to_dib(path: Path) -> bytes:
    width, height, channels, rows = decode_png(path)
    header = struct.pack(
        "<IiiHHIIiiII",
        40,
        width,
        height * 2,
        1,
        32,
        0,
        width * height * 4,
        0,
        0,
        0,
        0,
    )
    xor = bytearray()
    and_mask = bytearray()
    and_stride = ((width + 31) // 32) * 4
    for row in reversed(rows):
        rgba = bytearray()
        mask_row = bytearray(and_stride)
        for x in range(width):
            base = x * channels
            if channels == 4:
                r, g, b, a = row[base:base + 4]
            else:
                r, g, b = row[base:base + 3]
                a = 255
            rgba.extend((b, g, r, a))
            if a == 0:
                mask_row[x // 8] |= 0x80 >> (x % 8)
        xor.extend(rgba)
        and_mask.extend(mask_row)
    return header + bytes(xor) + bytes(and_mask)


root = Path(__file__).resolve().parent.parent / "src-tauri" / "icons"
ico_images = []
for size in (16, 20, 24, 32, 48, 64):
    ico_images.append((size, png_to_dib(root / f"{size}x{size}.png")))
for size, name in ((128, "128x128.png"), (256, "icon.png")):
    ico_images.append((size, (root / name).read_bytes()))

header = struct.pack("<HHH", 0, 1, len(ico_images))
offset = 6 + 16 * len(ico_images)
entries = []
payload = []
for size, image in ico_images:
    entries.append(struct.pack("<BBBBHHII", size if size < 256 else 0, size if size < 256 else 0, 0, 0, 1, 32, len(image), offset))
    offset += len(image)
    payload.append(image)
(root / "icon.ico").write_bytes(header + b"".join(entries) + b"".join(payload))

chunks = []
for tag, name in (
    ("icp4", "16x16.png"),
    ("icp5", "32x32.png"),
    ("ic06", "64x64.png"),
    ("ic07", "128x128.png"),
    ("ic08", "icon.png"),
):
    image = (root / name).read_bytes()
    chunks.append(tag.encode("ascii") + struct.pack(">I", len(image) + 8) + image)
(root / "icon.icns").write_bytes(b"icns" + struct.pack(">I", 8 + sum(len(chunk) for chunk in chunks)) + b"".join(chunks))
