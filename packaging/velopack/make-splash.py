# -*- coding: utf-8 -*-
"""Regenerate the Velopack splash image (splash.png).

Reuses the brand background from packaging/brand-art.py.
"""
import importlib.util
import os

HERE = os.path.dirname(os.path.abspath(__file__))
SPEC = importlib.util.spec_from_file_location(
    "art", os.path.join(HERE, "..", "brand-art.py")
)
art = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(art)

from PIL import ImageDraw  # noqa: E402

img = art.base()
art.paste_cloud(img, 0.300, int(0.24 * art.H))
d = ImageDraw.Draw(img)
d.text((0.5 * art.W, 0.52 * art.H), "Pandock", font=art.font(art.FONT_BOLD, 95),
       fill=(255, 255, 255), anchor="mm")
d.text((0.5 * art.W, 0.615 * art.H), "你的文件，在更好的位置",
       font=art.font(art.FONT_REG, 36), fill=art.SUB, anchor="mm")
out = os.path.join(HERE, "splash.png")
img.convert("RGB").save(out)
print("splash:", out)

