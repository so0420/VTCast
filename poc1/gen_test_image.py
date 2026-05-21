"""
Generate a synthetic RGBA test image that stresses the alpha pipeline.

Edge cases under test:
  - Solid alpha core (alpha=255)
  - Soft anti-aliased silhouette edge (alpha gradient 0..255)
  - Sharp alpha cut-outs (binary 0 or 255)
  - Translucent regions (alpha=128) overlapping bright colors
  - Saturated colors at low alpha (worst case for premultiplied vs straight)

If H.264 side-by-side preserves this, it will preserve a VTuber character.
"""
from PIL import Image, ImageDraw, ImageFilter
import math

W, H = 1280, 720
img = Image.new("RGBA", (W, H), (0, 0, 0, 0))
draw = ImageDraw.Draw(img)

# 1. Skin-tone head (large feathered ellipse) — soft alpha edge
head_layer = Image.new("RGBA", (W, H), (0, 0, 0, 0))
hd = ImageDraw.Draw(head_layer)
hd.ellipse((W//2 - 220, H//2 - 280, W//2 + 220, H//2 + 200), fill=(255, 218, 195, 255))
head_layer = head_layer.filter(ImageFilter.GaussianBlur(radius=2.5))
img = Image.alpha_composite(img, head_layer)

# 2. Hair (saturated pink, hard edge mostly, fine wisps at top)
hair_layer = Image.new("RGBA", (W, H), (0, 0, 0, 0))
hl = ImageDraw.Draw(hair_layer)
hl.pieslice((W//2 - 240, H//2 - 320, W//2 + 240, H//2 + 40), 180, 360, fill=(255, 90, 160, 255))
# Fine wisps — vertical lines extending up, semi-transparent
for i, x_off in enumerate([-180, -120, -60, 30, 90, 160]):
    a = 180 - i * 10
    hl.line([(W//2 + x_off, H//2 - 270), (W//2 + x_off - 5, H//2 - 340)],
            fill=(255, 90, 160, a), width=4)
img = Image.alpha_composite(img, hair_layer)

# 3. Eyes — sharp dark spots (test small-feature alpha)
draw = ImageDraw.Draw(img)
draw.ellipse((W//2 - 110, H//2 - 70, W//2 - 50, H//2 - 10), fill=(40, 70, 180, 255))
draw.ellipse((W//2 + 50, H//2 - 70, W//2 + 110, H//2 - 10), fill=(40, 70, 180, 255))

# 4. Translucent halo (alpha=80, saturated cyan) — worst case for chroma subsampling
halo_layer = Image.new("RGBA", (W, H), (0, 0, 0, 0))
ha = ImageDraw.Draw(halo_layer)
for r in range(280, 340, 2):
    ha.ellipse((W//2 - r, H//2 - r - 40, W//2 + r, H//2 + r - 40),
               outline=(0, 220, 255, 80), width=2)
halo_layer = halo_layer.filter(ImageFilter.GaussianBlur(radius=1.5))
img = Image.alpha_composite(img, halo_layer)

# 5. Sharp accessory — yellow star at corner (test sharp alpha edges)
def star_points(cx, cy, r_outer, r_inner, n=5):
    pts = []
    for i in range(n * 2):
        r = r_outer if i % 2 == 0 else r_inner
        a = -math.pi / 2 + i * math.pi / n
        pts.append((cx + r * math.cos(a), cy + r * math.sin(a)))
    return pts

draw.polygon(star_points(W//2 + 180, H//2 - 180, 40, 18), fill=(255, 220, 60, 255))

# 6. Gradient strip on the left edge — test alpha gradient through encoding
for y in range(H):
    a = int(255 * (y / H))
    for x in range(20):
        img.putpixel((x, y), (200, 50, 200, a))

img.save("test_input.png")
print(f"Wrote test_input.png  {W}x{H} RGBA")

# Save an opaque preview too — what the character "looks like" on a checkerboard
preview = Image.new("RGB", (W, H), (255, 255, 255))
# Draw checkerboard
for y in range(0, H, 32):
    for x in range(0, W, 32):
        if (x // 32 + y // 32) % 2 == 0:
            for dy in range(32):
                for dx in range(32):
                    if x + dx < W and y + dy < H:
                        preview.putpixel((x + dx, y + dy), (200, 200, 200))
preview.paste(img, (0, 0), img)
preview.save("test_input_preview.png")
print("Wrote test_input_preview.png (on checkerboard for reference)")
