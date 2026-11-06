#!/usr/bin/env python3
"""Export Microsoft Store artwork from the original icon (requires Pillow)."""

from pathlib import Path

from PIL import Image, ImageOps


ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "src/assets/icon.png"
OUTPUT = ROOT / "packaging/windows"
BACKGROUND = (20, 24, 31, 255)

LISTING_IMAGES = (
    ("app-tile", (71, 71)),
    ("app-tile", (150, 150)),
    ("app-tile", (300, 300)),
    ("box-art", (1080, 1080)),
    ("box-art", (2160, 2160)),
    ("poster-art", (720, 1080)),
    ("poster-art", (1440, 2160)),
)
PACKAGE_ICONS = (
    ("StoreLogo", 50),
    ("Square44x44Logo", 44),
    ("Square150x150Logo", 150),
)


def save(image, destination):
    destination.parent.mkdir(parents=True, exist_ok=True)
    image.save(destination, format="PNG", optimize=True)
    print(destination.relative_to(ROOT))


def main():
    with Image.open(SOURCE) as source:
        icon = source.convert("RGBA")

    for kind, (width, height) in LISTING_IMAGES:
        tile = kind == "app-tile"
        canvas = Image.new(
            "RGBA", (width, height), (0, 0, 0, 0) if tile else BACKGROUND
        )
        inset = 0 if tile else round(width * 0.1)
        artwork = ImageOps.contain(
            icon,
            (width - 2 * inset, height - 2 * inset),
            method=Image.Resampling.LANCZOS,
        )
        canvas.alpha_composite(
            artwork, ((width - artwork.width) // 2, (height - artwork.height) // 2)
        )
        if not tile:
            canvas = canvas.convert("RGB")
        save(
            canvas,
            OUTPUT / "store-listing" / f"canvaz-{kind}-{width}x{height}.png",
        )

    for name, size in PACKAGE_ICONS:
        for scale in (1, 2):
            filename = f"{name}.png" if scale == 1 else f"{name}.scale-200.png"
            artwork = ImageOps.contain(
                icon, (size * scale, size * scale), method=Image.Resampling.LANCZOS
            )
            save(artwork, OUTPUT / "msix/Assets" / filename)


if __name__ == "__main__":
    main()
