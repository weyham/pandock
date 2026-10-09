"""Generate Pandock tray status icons: flat cloud glyph + top-right status dot.

Dot sizing follows the Halcyon tray spec:
    r = max(2, round(size * 0.0833))   -> 32px: 3  (6px diameter)
    m = round(size * 0.03)             -> 32px: 1

Position note: Halcyon anchors the dot to the tile corner because its glyph sits
on an opaque rounded plate. The Pandock tray glyph has no plate, so the dot is
pulled inward (DOT_PULL_X / DOT_PULL_Y) to hug the cloud's top-right shoulder
instead of floating in empty space. Sizing is unchanged.
"""
from __future__ import annotations

import argparse
import os
import sys

from PIL import Image, ImageDraw

DEFAULT_SIZE = 32
CLOUD_SCALE = 1.06
DOT_PULL_X = 0.05
DOT_PULL_Y = 0.03
STATUS_COLORS = {
    "green": (34, 197, 94, 255),
    "yellow": (234, 179, 8, 255),
    "red": (239, 68, 68, 255),
}
BACKING = (6, 14, 24, 230)


def dot_geometry(size: int) -> tuple[int, int]:
    radius = max(2, round(size * 0.0833))
    margin = round(size * 0.03)
    cx = min(
        size - radius - 1,
        size - radius - margin - round(size * DOT_PULL_X),
    )
    cy = max(
        radius + 1,
        radius + margin + round(size * DOT_PULL_Y),
    )
    return radius, cx, cy


def build_tile(cloud: Image.Image, size: int, color: tuple[int, int, int, int]) -> Image.Image:
    tile = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    target = max(1, round(size * CLOUD_SCALE))
    scaled = cloud.copy()
    scaled.thumbnail((target, target), Image.LANCZOS)
    x = (size - scaled.width) // 2
    y = size - scaled.height - max(0, round(size * 0.03))
    tile.paste(scaled, (x, y), scaled)

    radius, cx, cy = dot_geometry(size)
    draw = ImageDraw.Draw(tile)
    draw.ellipse(
        [cx - radius - 1, cy - radius - 1, cx + radius + 1, cy + radius + 1],
        fill=BACKING,
    )
    draw.ellipse([cx - radius, cy - radius, cx + radius, cy + radius], fill=color)
    return tile


def render_preview(cloud: Image.Image, path: str) -> None:
    sizes = (32, 24, 16)
    pad = 12
    width = pad + sum(s + pad for s in sizes)
    height = (32 + pad) * 2 + pad
    canvas = Image.new("RGBA", (width, height), (0, 0, 0, 0))
    for row, bg in enumerate(((32, 32, 32, 255), (243, 243, 243, 255))):
        band = Image.new("RGBA", (width, 32 + pad), bg)
        x = pad
        for size in sizes:
            tile = build_tile(cloud, size, STATUS_COLORS["green"])
            band.paste(tile, (x, (band.height - size) // 2), tile)
            x += size + pad
        canvas.paste(band, (0, pad + row * (32 + pad)))
    canvas.resize((width * 3, height * 3), Image.NEAREST).save(path)
    print(f"wrote preview {path}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cloud", required=True, help="transparent cloud PNG (any large size)")
    parser.add_argument("--out-dir", required=True)
    parser.add_argument("--size", type=int, default=DEFAULT_SIZE)
    parser.add_argument("--preview", help="optional path for a dark/light legibility preview")
    args = parser.parse_args()

    cloud = Image.open(args.cloud).convert("RGBA")
    bbox = cloud.getbbox()
    if bbox:
        cloud = cloud.crop(bbox)

    os.makedirs(args.out_dir, exist_ok=True)
    for name, color in STATUS_COLORS.items():
        tile = build_tile(cloud, args.size, color)
        path = os.path.join(args.out_dir, f"status-{name}.png")
        tile.save(path)
        print(f"wrote {path} ({tile.width}x{tile.height})")

    if args.preview:
        render_preview(cloud, args.preview)
    return 0


if __name__ == "__main__":
    sys.exit(main())
