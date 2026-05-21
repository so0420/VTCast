//! CPU box-filter RGBA downsampler.
//!
//! Used by the pipeline to keep the packed (side-by-side) frame width
//! within the encoder's supported maximum. NVENC H.264 caps at 4096,
//! and libx264 / Media Foundation accept larger but slow down hard; the
//! pipeline defaults to capping packed width at 4096 (source ≤ 2048).
//!
//! A box filter averages all source pixels falling into each destination
//! pixel's footprint. Cheap, visually fine for the 1.2x–2x downsample
//! ratios we usually see, and trivially parallelisable later if needed.

/// Decide the effective source dimensions so that `eff_w * 2 <= max_packed_w`
/// while preserving aspect ratio. Both returned dimensions are clamped to
/// even values (encoders reject odd dims).
pub fn compute_effective_dims(src_w: u32, src_h: u32, max_packed_w: u32) -> (u32, u32) {
    let max_src_w = max_packed_w / 2;
    if src_w <= max_src_w {
        return (src_w & !1, src_h & !1);
    }
    let scale = max_src_w as f64 / src_w as f64;
    let new_w = max_src_w & !1;
    let new_h = ((src_h as f64 * scale).round() as u32) & !1;
    (new_w, new_h.max(2))
}

/// Box-filter RGBA `src` (W=`src_w`, H=`src_h`) into `dst` (W=`dst_w`,
/// H=`dst_h`). Both buffers must be sized W*H*4. If `dst_w == src_w &&
/// dst_h == src_h`, this degenerates to a memcpy.
pub fn box_resize_rgba(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    dst: &mut [u8],
    dst_w: u32,
    dst_h: u32,
) {
    debug_assert_eq!(src.len(), (src_w * src_h * 4) as usize);
    debug_assert_eq!(dst.len(), (dst_w * dst_h * 4) as usize);

    if src_w == dst_w && src_h == dst_h {
        dst.copy_from_slice(src);
        return;
    }

    let sw = src_w as usize;
    let sh = src_h as usize;
    let dw = dst_w as usize;
    let dh = dst_h as usize;
    let xr = src_w as f64 / dst_w as f64;
    let yr = src_h as f64 / dst_h as f64;

    for dy in 0..dh {
        let sy0 = (dy as f64 * yr).floor() as usize;
        let sy1 = (((dy as f64 + 1.0) * yr).ceil() as usize).min(sh);
        let sy1 = sy1.max(sy0 + 1);
        let row_off = dy * dw * 4;

        for dx in 0..dw {
            let sx0 = (dx as f64 * xr).floor() as usize;
            let sx1 = (((dx as f64 + 1.0) * xr).ceil() as usize).min(sw);
            let sx1 = sx1.max(sx0 + 1);

            let (mut r, mut g, mut b, mut a, mut n) = (0u32, 0u32, 0u32, 0u32, 0u32);
            for y in sy0..sy1 {
                let row = y * sw * 4;
                for x in sx0..sx1 {
                    let i = row + x * 4;
                    r += src[i] as u32;
                    g += src[i + 1] as u32;
                    b += src[i + 2] as u32;
                    a += src[i + 3] as u32;
                    n += 1;
                }
            }
            let c = n.max(1);
            let di = row_off + dx * 4;
            dst[di] = (r / c) as u8;
            dst[di + 1] = (g / c) as u8;
            dst[di + 2] = (b / c) as u8;
            dst[di + 3] = (a / c) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_even_dims_within_cap() {
        // Already small: returned as-is but even-clamped
        assert_eq!(compute_effective_dims(1920, 1080, 4096), (1920, 1080));
        assert_eq!(compute_effective_dims(1921, 1081, 4096), (1920, 1080));
        // Over the cap (Warudo case in the user log): 2746 -> 2048, height
        // scaled proportionally and clamped even
        let (w, h) = compute_effective_dims(2746, 1534, 4096);
        assert_eq!(w, 2048);
        // 1534 * 2048 / 2746 ≈ 1144 (even)
        assert_eq!(h, 1144);
    }

    #[test]
    fn identity_resize_is_memcpy() {
        let src = vec![
            10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120, 130, 140, 150, 160,
        ];
        let mut dst = vec![0u8; src.len()];
        box_resize_rgba(&src, 2, 2, &mut dst, 2, 2);
        assert_eq!(dst, src);
    }

    #[test]
    fn averages_2x2_to_1x1() {
        let src = vec![
            0, 0, 0, 0,         // (0,0) black
            100, 100, 100, 100, // (1,0)
            200, 200, 200, 200, // (0,1)
            44, 44, 44, 44,     // (1,1)
        ];
        let mut dst = vec![0u8; 4];
        box_resize_rgba(&src, 2, 2, &mut dst, 1, 1);
        // average = (0 + 100 + 200 + 44) / 4 = 86 per channel
        assert_eq!(dst, vec![86, 86, 86, 86]);
    }
}
