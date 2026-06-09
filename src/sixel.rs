use image::RgbaImage;

const TRANSPARENT: u16 = u16::MAX;

pub fn encode(img: &RgbaImage, max_colors: usize) -> Vec<u8> {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return b"\x1bPq\x1b\\".to_vec();
    }

    let (palette, indexed) = quantize(img, max_colors);
    if palette.is_empty() {
        return b"\x1bPq\x1b\\".to_vec();
    }

    let w_usize = w as usize;
    let n_bands = ((h as usize) + 5) / 6;

    let mut out = Vec::with_capacity(1024 + w_usize * h as usize);

    out.extend_from_slice(b"\x1bPq");

    for (i, c) in palette.iter().enumerate() {
        let r = c[0] as u32 * 100 / 255;
        let g = c[1] as u32 * 100 / 255;
        let b = c[2] as u32 * 100 / 255;
        let s = format!("#{};2;{};{};{}", i, r, g, b);
        out.extend_from_slice(s.as_bytes());
    }

    for band in 0..n_bands {
        let y_start = band * 6;
        let y_count = 6.min(h as usize - y_start);

        let mut band_colors: Vec<usize> = Vec::new();
        {
            let mut used = vec![false; palette.len()];
            for x in 0..w_usize {
                for dy in 0..y_count {
                    let y = y_start + dy;
                    let idx = indexed[y * w_usize + x] as usize;
                    if idx < palette.len() && !used[idx] {
                        used[idx] = true;
                        band_colors.push(idx);
                    }
                }
            }
        }

        let mut first_in_band = true;
        for &cidx in &band_colors {
            if !first_in_band {
                out.push(b'$');
            }
            first_in_band = false;

            let cstr = format!("#{}", cidx);
            out.extend_from_slice(cstr.as_bytes());

            let mut x = 0;
            while x < w_usize {
                let mut sixel = 0u8;
                for dy in 0..y_count {
                    let y = y_start + dy;
                    if indexed[y * w_usize + x] as usize == cidx {
                        sixel |= 1 << dy;
                    }
                }

                let mut run = 1;
                while x + run < w_usize {
                    let mut next = 0u8;
                    for dy in 0..y_count {
                        let y = y_start + dy;
                        if indexed[y * w_usize + x + run] as usize == cidx {
                            next |= 1 << dy;
                        }
                    }
                    if next != sixel {
                        break;
                    }
                    run += 1;
                }

                let ch = sixel + 63;
                if run >= 3 {
                    let rle = format!("!{}{}", run, ch as char);
                    out.extend_from_slice(rle.as_bytes());
                } else {
                    for _ in 0..run {
                        out.push(ch);
                    }
                }

                x += run;
            }
        }
        out.push(b'-');
    }

    out.extend_from_slice(b"\x1b\\");
    out
}

fn quantize(img: &RgbaImage, max_colors: usize) -> (Vec<[u8; 3]>, Vec<u16>) {
    let (w, h) = img.dimensions();
    let n = (w * h) as usize;
    let max_colors = max_colors.clamp(4, 65534);

    let is_transparent: Vec<bool> = img.pixels().map(|p| p.0[3] < 240).collect();

    let opaque_rgba: Vec<u8> = img
        .pixels()
        .filter(|p| p.0[3] >= 128)
        .flat_map(|p| p.0)
        .collect();

    if opaque_rgba.is_empty() {
        return (vec![[0, 0, 0]], vec![TRANSPARENT; n]);
    }

    if max_colors <= 256 {
        quantize_neuquant(img, &is_transparent, &opaque_rgba, max_colors)
    } else {
        quantize_uniform(img, &is_transparent, max_colors)
    }
}

fn quantize_neuquant(
    img: &RgbaImage,
    is_transparent: &[bool],
    opaque_rgba: &[u8],
    max_colors: usize,
) -> (Vec<[u8; 3]>, Vec<u16>) {
    use color_quant::NeuQuant;

    let (w, h) = img.dimensions();
    let wu = w as usize;
    let n = (w * h) as usize;

    let nq = NeuQuant::new(10, max_colors.min(256), opaque_rgba);

    let pal = nq.color_map_rgb();
    let palette: Vec<[u8; 3]> = pal.chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect();

    let mut indexed = vec![TRANSPARENT; n];
    let mut errors = vec![[0.0f32; 3]; n];

    for y in 0..h as usize {
        for x in 0..wu {
            let i = y * wu + x;
            if is_transparent[i] {
                continue;
            }

            let p = img.get_pixel(x as u32, y as u32);
            let err = errors[i];

            let r = (p.0[0] as f32 + err[0]).clamp(0.0, 255.0);
            let g = (p.0[1] as f32 + err[1]).clamp(0.0, 255.0);
            let b = (p.0[2] as f32 + err[2]).clamp(0.0, 255.0);

            let rgba = [r as u8, g as u8, b as u8, 255u8];
            let idx = nq.index_of(&rgba);
            indexed[i] = idx as u16;

            let c = palette[idx];
            let diff_r = r - c[0] as f32;
            let diff_g = g - c[1] as f32;
            let diff_b = b - c[2] as f32;

            // Atkinson Error Matrix (diffuses 1/8 to 6 neighboring pixels)
            let err_r = diff_r * 0.125;
            let err_g = diff_g * 0.125;
            let err_b = diff_b * 0.125;

            let x_right1 = x + 1;
            let x_right2 = x + 2;
            let x_left1 = x.saturating_sub(1);
            let y_down1 = y + 1;
            let y_down2 = y + 2;

            if x_right1 < wu {
                let next_i = y * wu + x_right1;
                errors[next_i][0] += err_r;
                errors[next_i][1] += err_g;
                errors[next_i][2] += err_b;
            }
            if x_right2 < wu {
                let next_i = y * wu + x_right2;
                errors[next_i][0] += err_r;
                errors[next_i][1] += err_g;
                errors[next_i][2] += err_b;
            }
            if y_down1 < h as usize {
                if x > 0 {
                    let next_i = y_down1 * wu + x_left1;
                    errors[next_i][0] += err_r;
                    errors[next_i][1] += err_g;
                    errors[next_i][2] += err_b;
                }
                let next_i = y_down1 * wu + x;
                errors[next_i][0] += err_r;
                errors[next_i][1] += err_g;
                errors[next_i][2] += err_b;

                if x_right1 < wu {
                    let next_i = y_down1 * wu + x_right1;
                    errors[next_i][0] += err_r;
                    errors[next_i][1] += err_g;
                    errors[next_i][2] += err_b;
                }
            }
            if y_down2 < h as usize {
                let next_i = y_down2 * wu + x;
                errors[next_i][0] += err_r;
                errors[next_i][1] += err_g;
                errors[next_i][2] += err_b;
            }
        }
    }

    (palette, indexed)
}

fn quantize_uniform(
    img: &RgbaImage,
    is_transparent: &[bool],
    max_colors: usize,
) -> (Vec<[u8; 3]>, Vec<u16>) {
    let (w, h) = img.dimensions();
    let wu = w as usize;
    let n = (w * h) as usize;

    let n_levels = ((max_colors as f64).cbrt().round() as u32).max(2);
    let max_val = n_levels - 1;
    let step = 255.0 / max_val as f32;

    let mut palette = Vec::with_capacity((n_levels * n_levels * n_levels) as usize);
    for ri in 0..n_levels {
        for gi in 0..n_levels {
            for bi in 0..n_levels {
                palette.push([
                    (ri as f32 * step).round() as u8,
                    (gi as f32 * step).round() as u8,
                    (bi as f32 * step).round() as u8,
                ]);
            }
        }
    }

    let max_val_f = max_val as f32;
    let n_levels_sq = n_levels * n_levels;

    let mut indexed = vec![TRANSPARENT; n];
    let mut errors = vec![[0.0f32; 3]; n];

    for y in 0..h as usize {
        for x in 0..wu {
            let i = y * wu + x;
            if is_transparent[i] {
                continue;
            }

            let p = img.get_pixel(x as u32, y as u32);
            let err = errors[i];

            let r = (p.0[0] as f32 + err[0]).clamp(0.0, 255.0);
            let g = (p.0[1] as f32 + err[1]).clamp(0.0, 255.0);
            let b = (p.0[2] as f32 + err[2]).clamp(0.0, 255.0);

            let ri = (r / step).round().clamp(0.0, max_val_f) as u32;
            let gi = (g / step).round().clamp(0.0, max_val_f) as u32;
            let bi = (b / step).round().clamp(0.0, max_val_f) as u32;

            let pal_idx = (ri * n_levels_sq + gi * n_levels + bi) as usize;
            indexed[i] = pal_idx as u16;

            let c = palette[pal_idx];
            let diff_r = r - c[0] as f32;
            let diff_g = g - c[1] as f32;
            let diff_b = b - c[2] as f32;

            let err_r = diff_r * 0.125;
            let err_g = diff_g * 0.125;
            let err_b = diff_b * 0.125;

            let x_right1 = x + 1;
            let x_right2 = x + 2;
            let x_left1 = x.saturating_sub(1);
            let y_down1 = y + 1;
            let y_down2 = y + 2;

            if x_right1 < wu {
                let next_i = y * wu + x_right1;
                errors[next_i][0] += err_r;
                errors[next_i][1] += err_g;
                errors[next_i][2] += err_b;
            }
            if x_right2 < wu {
                let next_i = y * wu + x_right2;
                errors[next_i][0] += err_r;
                errors[next_i][1] += err_g;
                errors[next_i][2] += err_b;
            }
            if y_down1 < h as usize {
                if x > 0 {
                    let next_i = y_down1 * wu + x_left1;
                    errors[next_i][0] += err_r;
                    errors[next_i][1] += err_g;
                    errors[next_i][2] += err_b;
                }
                let next_i = y_down1 * wu + x;
                errors[next_i][0] += err_r;
                errors[next_i][1] += err_g;
                errors[next_i][2] += err_b;

                if x_right1 < wu {
                    let next_i = y_down1 * wu + x_right1;
                    errors[next_i][0] += err_r;
                    errors[next_i][1] += err_g;
                    errors[next_i][2] += err_b;
                }
            }
            if y_down2 < h as usize {
                let next_i = y_down2 * wu + x;
                errors[next_i][0] += err_r;
                errors[next_i][1] += err_g;
                errors[next_i][2] += err_b;
            }
        }
    }

    (palette, indexed)
}
