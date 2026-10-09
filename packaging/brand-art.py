# -*- coding: utf-8 -*-
"""Shared Pandock brand background art (used by packaging generators).

Renders the dark navy gradient base with decorative blurred ellipses and
the cloud icon. Layout is 2x of an 880x495 surface.
"""
import os
from PIL import Image, ImageDraw, ImageFilter, ImageFont

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, ".."))

W, H = 1760, 990

TOP = (11, 22, 46)
BOTTOM = (16, 33, 66)
BTN_TOP = (59, 140, 255)
BTN_BOTTOM = (31, 111, 230)
SUB = (157, 180, 220)
LINK = (126, 168, 242)
FOOT = (78, 97, 136)

FONT_BOLD = r"C:\Windows\Fonts\msyhbd.ttc"
FONT_REG = r"C:\Windows\Fonts\msyh.ttc"


def font(path, size):
    return ImageFont.truetype(path, size)


def vgrad(size, top, bottom):
    w, h = size
    img = Image.new("RGB", size)
    px = img.load()
    for y in range(h):
        t = y / (h - 1)
        row = tuple(int(top[i] + (bottom[i] - top[i]) * t) for i in range(3))
        for x in range(w):
            px[x, y] = row
    return img


def base():
    img = vgrad((W, H), TOP, BOTTOM).convert("RGBA")
    deco = Image.new("RGBA", (W, H), (0, 0, 0, 0))
    d = ImageDraw.Draw(deco)
    d.ellipse([0.62 * W, -0.35 * H, 1.42 * W, 0.45 * H], fill=(29, 62, 120, 55))
    d.ellipse([-0.46 * W, 0.62 * H, 0.34 * W, 1.42 * H], fill=(23, 50, 99, 70))
    deco = deco.filter(ImageFilter.GaussianBlur(110))
    img.alpha_composite(deco)
    return img


def paste_cloud(img, cy, ch):
    icon = Image.open(os.path.join(ROOT, "src-tauri", "icons", "icon.png")).convert("RGBA")
    icon = icon.resize((ch, ch), Image.LANCZOS)
    img.alpha_composite(icon, (int(0.5 * W - ch / 2), int(cy * H - ch / 2)))
