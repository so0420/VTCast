"""
Programmatic verification of the side-by-side alpha pipeline.

Compares the original test_input.png against the reconstructed RGBA built from
packed_decoded.png (= H.264 round-tripped side-by-side frame).

Metrics:
  - alpha MAE   (mean absolute error, 0..255)
  - rgb MAE     (in regions where original alpha > 32, scored straight RGB)
  - edge bleed  (alpha error specifically near alpha discontinuities)
  - alpha histogram of errors
"""
import numpy as np
from PIL import Image

orig = np.array(Image.open("test_input.png").convert("RGBA"))
packed = np.array(Image.open("packed_decoded.png").convert("RGB"))

H, W2, _ = packed.shape
W = W2 // 2

rgb_decoded   = packed[:, :W, :]              # left half
alpha_decoded = packed[:, W:, 0]              # right half R-channel

recon = np.dstack([rgb_decoded, alpha_decoded])
Image.fromarray(recon, "RGBA").save("reconstructed.png")

orig_a = orig[..., 3].astype(np.int16)
rec_a  = alpha_decoded.astype(np.int16)
a_err  = np.abs(orig_a - rec_a)
print(f"alpha MAE         : {a_err.mean():.2f} / 255   ({a_err.mean()/255*100:.2f}%)")
print(f"alpha max error   : {a_err.max():>3d} / 255")
print(f"alpha pixels >16  : {(a_err > 16).mean()*100:.2f}% of frame")

# RGB error only where original is meaningfully opaque
mask = orig[..., 3] > 32
orig_rgb = orig[..., :3].astype(np.int16)
rec_rgb  = rgb_decoded.astype(np.int16)
rgb_err  = np.abs(orig_rgb - rec_rgb)
rgb_mae  = rgb_err[mask].mean()
print(f"rgb   MAE (opaque): {rgb_mae:.2f} / 255   ({rgb_mae/255*100:.2f}%)")

# Edge bleed: alpha error near alpha discontinuities (numpy-only)
dx = np.abs(np.diff(orig_a, axis=1, prepend=0))
dy = np.abs(np.diff(orig_a, axis=0, prepend=0))
edge_mask = (dx + dy) > 40
if edge_mask.any():
    print(f"alpha MAE @ edges : {a_err[edge_mask].mean():.2f} / 255")
    print(f"rgb   MAE @ edges : {rgb_err[edge_mask].mean():.2f} / 255")

# Histogram of alpha errors
print("\nalpha error histogram:")
bins = [0, 1, 2, 4, 8, 16, 32, 64, 256]
hist, _ = np.histogram(a_err, bins=bins)
for i in range(len(bins) - 1):
    pct = hist[i] / a_err.size * 100
    bar = "#" * min(int(pct), 50)
    print(f"  err {bins[i]:>3d}-{bins[i+1]-1:<3d}  {pct:5.2f}%  {bar}")
