"""Render the app icon (equaliser bars in a listening ring) to packaging/icon.{png,ico,icns}.

Run with: python packaging/make_icon.py   (needs Pillow)
"""
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter

S = 1024  # master size; drawn at 4x and downsampled for smooth edges
SS = S * 4
ACCENT = (0x5B, 0x7C, 0xFF)
ACCENT_2 = (0x9B, 0x5C, 0xFF)
OUT = Path(__file__).parent


def render():
    # Diagonal accent gradient, same pair the Identify button pulses between.
    v = Image.linear_gradient("L")
    px = Image.blend(v, v.rotate(90), 0.5).transpose(Image.FLIP_LEFT_RIGHT).resize((SS, SS), Image.BICUBIC)
    grad = Image.composite(Image.new("RGB", (SS, SS), ACCENT_2), Image.new("RGB", (SS, SS), ACCENT), px)

    # Rounded-square tile with a margin, macOS-style proportions.
    m = SS * 0.09
    mask = Image.new("L", (SS, SS), 0)
    ImageDraw.Draw(mask).rounded_rectangle((m, m, SS - m, SS - m), radius=SS * 0.2, fill=255)
    tile = Image.new("RGBA", (SS, SS), (0, 0, 0, 0))
    tile.paste(grad, (0, 0), mask)

    # Soft drop shadow under the tile.
    shadow = Image.new("RGBA", (SS, SS), (0, 0, 0, 0))
    ImageDraw.Draw(shadow).rounded_rectangle((m, m + SS * 0.015, SS - m, SS - m + SS * 0.015), radius=SS * 0.2, fill=(0, 0, 0, 90))
    shadow = shadow.filter(ImageFilter.GaussianBlur(SS * 0.012))
    img = Image.alpha_composite(shadow, tile)

    c = SS / 2
    # Two faint "listening" rings around the bars (own layer so alpha blends).
    rings = Image.new("RGBA", (SS, SS), (0, 0, 0, 0))
    rd = ImageDraw.Draw(rings)
    for r, a in ((SS * 0.27, 80), (SS * 0.34, 40)):
        rd.ellipse((c - r, c - r, c + r, c + r), outline=(255, 255, 255, a), width=round(SS * 0.012))
    img = Image.alpha_composite(img, rings)

    d = ImageDraw.Draw(img)
    # Five-bar equaliser glyph, matching eq_icon() in src/app.rs.
    heights = [0.45, 0.8, 1.0, 0.65, 0.35]
    w, h = SS * 0.34, SS * 0.36
    gap = w / (len(heights) * 2 - 1)
    left = c - w / 2
    for i, f in enumerate(heights):
        x = left + i * gap * 2
        bh = h * f
        d.rounded_rectangle((x, c - bh / 2, x + gap, c + bh / 2), radius=gap / 2, fill=(255, 255, 255, 255))

    return img.resize((S, S), Image.LANCZOS)


if __name__ == "__main__":
    icon = render()
    icon.save(OUT / "icon.png")
    icon.save(OUT / "icon.ico", sizes=[(s, s) for s in (16, 24, 32, 48, 64, 128, 256)])
    icon.save(OUT / "icon.icns")
    print("wrote", ", ".join(str(OUT / f"icon.{e}") for e in ("png", "ico", "icns")))
