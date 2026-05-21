//! Side-by-side alpha packing.
//!
//! Input:  W×H RGBA from Spout.
//! Output: (2W)×H RGBA, where the left half copies the source RGB and the
//!         right half replicates the source alpha into R, G, and B. The
//!         encoder discards the output's alpha channel; we only care about
//!         the RGB it carries through. The receiver's WebGL shader samples
//!         the left half as RGB and the right half's R as alpha.

pub fn packed_dims(src_w: u32, src_h: u32) -> (u32, u32) {
    (src_w * 2, src_h)
}

/// Chroma-key settings used to extract a transparent foreground from a
/// captured RGBA frame that doesn't ship with native alpha (WGC, DDA).
///
/// Distance to the key color is measured as the Chebyshev (max-channel)
/// distance — simple, fast, and good enough for "remove this green
/// background" use cases. Pixels within `threshold` of the key become
/// fully transparent; pixels in the `softness` band beyond that fade
/// linearly back to fully opaque.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct ChromaKey {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    /// 0..=255. Anything within this distance of the key is keyed out.
    pub threshold: u8,
    /// 0..=255. Width of the soft feathered ramp beyond `threshold`.
    pub softness: u8,
}

impl Default for ChromaKey {
    fn default() -> Self {
        // OBS-style "green screen" default.
        Self {
            r: 0,
            g: 255,
            b: 0,
            threshold: 60,
            softness: 30,
        }
    }
}

/// Rewrite the alpha plane of an RGBA buffer in place using `key`.
///
/// `rgba.len()` must be a multiple of 4.
pub fn apply_chroma_key(rgba: &mut [u8], key: &ChromaKey) {
    let kr = key.r as i32;
    let kg = key.g as i32;
    let kb = key.b as i32;
    let t = key.threshold as i32;
    let s = (key.softness as i32).max(1);
    for px in rgba.chunks_exact_mut(4) {
        let dr = (px[0] as i32 - kr).abs();
        let dg = (px[1] as i32 - kg).abs();
        let db = (px[2] as i32 - kb).abs();
        let dist = dr.max(dg).max(db);
        let new_alpha = if dist <= t {
            0
        } else if dist <= t + s {
            // Linear ramp from 0 (at t) to 255 (at t+s)
            (((dist - t) * 255) / s) as u8
        } else {
            255
        };
        px[3] = new_alpha;
    }
}

pub fn pack_rgba_side_by_side(src: &[u8], src_w: u32, src_h: u32, dst: &mut [u8]) {
    let sw = src_w as usize;
    let sh = src_h as usize;
    let dw = sw * 2;
    debug_assert_eq!(src.len(), sw * sh * 4);
    debug_assert_eq!(dst.len(), dw * sh * 4);

    for y in 0..sh {
        let src_row = &src[y * sw * 4..(y + 1) * sw * 4];
        let dst_row = &mut dst[y * dw * 4..(y + 1) * dw * 4];
        let (left, right) = dst_row.split_at_mut(sw * 4);
        for x in 0..sw {
            let s = &src_row[x * 4..x * 4 + 4];
            let (r, g, b, a) = (s[0], s[1], s[2], s[3]);
            left[x * 4] = r;
            left[x * 4 + 1] = g;
            left[x * 4 + 2] = b;
            left[x * 4 + 3] = 255;
            right[x * 4] = a;
            right[x * 4 + 1] = a;
            right[x * 4 + 2] = a;
            right[x * 4 + 3] = 255;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chroma_key_green_screen() {
        let mut frame = vec![
            0, 255, 0, 255,   // exact green key → alpha 0
            255, 0, 0, 255,   // far from green → alpha 255
            10, 240, 10, 255, // close to green within threshold 60 → alpha 0
            80, 200, 80, 255, // in soft band (dist=80, t=60, s=30 → ramp)
        ];
        let key = ChromaKey { r: 0, g: 255, b: 0, threshold: 60, softness: 30 };
        apply_chroma_key(&mut frame, &key);
        assert_eq!(frame[3], 0, "green key → 0");
        assert_eq!(frame[7], 255, "red → 255");
        assert_eq!(frame[11], 0, "near-green within threshold → 0");
        // 4th pixel: max channel diff = max(80,55,80) = 80
        // threshold 60, dist=80, s=30 → ramp: (80-60)*255/30 = 170
        assert_eq!(frame[15], 170, "soft band ramp");
    }

    #[test]
    fn packs_correctly() {
        // 2x1 source: red opaque pixel, then transparent green.
        let src = vec![
            255, 0, 0, 255, // (0,0) red, a=255
              0, 255, 0,   0, // (1,0) green, a=0
        ];
        let (dw, dh) = packed_dims(2, 1);
        assert_eq!((dw, dh), (4, 1));
        let mut dst = vec![0u8; (dw * dh * 4) as usize];
        pack_rgba_side_by_side(&src, 2, 1, &mut dst);
        assert_eq!(&dst[0..4], &[255, 0, 0, 255]);   // left[0] = src.rgb
        assert_eq!(&dst[4..8], &[0, 255, 0, 255]);   // left[1] = src.rgb
        assert_eq!(&dst[8..12], &[255, 255, 255, 255]); // right[0] = src.a
        assert_eq!(&dst[12..16], &[0, 0, 0, 255]);   // right[1] = src.a
    }
}
