//! Screens an app hides from capture.
//!
//! Apps such as PayPay mark their content as protected: iOS hands every
//! capture path (WDA screenshots, the MJPEG/H.264 view, Mirroring) a blank
//! area there, while the same screen's accessibility tree stays complete. A
//! blank image let agents conclude "still loading" or fall back to vision on
//! nothing. When a screenshot's content band is one flat colour while the tree
//! has labelled elements in it, the daemon draws the tree over the blank area
//! instead: outlines, kinds and labels, with a banner saying so. The pixels
//! themselves cannot and should not be recovered.

use crate::wda::ElementRow;

/// Fraction of the screen height kept clear of the analysis at the top
/// (status bar, navigation) and bottom (tab bar, home indicator): apps leave
/// those unprotected, so they would make a protected screen look non-blank.
const BAND_TOP: f64 = 0.07;
const BAND_BOTTOM: f64 = 0.88;
/// Share of sampled pixels that must match the dominant colour.
const BLANK_SHARE: f64 = 0.985;
/// Per-channel distance still counted as "the same colour".
const SAME_COLOUR: i32 = 10;
/// Labelled rows inside the band needed to call a blank screen protected
/// rather than genuinely empty.
const MIN_LABELLED_ROWS: usize = 3;

/// A decoded RGBA8 screenshot.
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub fn decode_png(bytes: &[u8]) -> Option<Image> {
    let mut decoder = png::Decoder::new(bytes);
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::GrayscaleAlpha => buf
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Indexed => return None,
    };
    Some(Image {
        width: info.width,
        height: info.height,
        rgba,
    })
}

pub fn encode_png(image: &Image) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, image.width, image.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder.write_header().ok()?;
        writer.write_image_data(&image.rgba).ok()?;
    }
    Some(out)
}

pub fn decode_jpeg(bytes: &[u8]) -> Option<Image> {
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(bytes, options);
    decoder.decode_headers().ok()?;
    let (width, height) = decoder.dimensions()?;
    let rgba = decoder.decode().ok()?;
    (rgba.len() == width * height * 4).then_some(Image {
        width: width as u32,
        height: height as u32,
        rgba,
    })
}

/// Shrink so the longer side is at most `max_side`, averaging each source
/// box (text stays legible where nearest-neighbour would drop strokes).
/// Never enlarges.
pub fn fit_within(image: Image, max_side: u32) -> Image {
    let long = image.width.max(image.height);
    if long <= max_side || max_side == 0 {
        return image;
    }
    let scale = f64::from(max_side) / f64::from(long);
    let width = ((f64::from(image.width) * scale).round() as u32).max(1);
    let height = ((f64::from(image.height) * scale).round() as u32).max(1);
    let (sw, sh) = (image.width as usize, image.height as usize);
    let mut rgba = vec![0u8; width as usize * height as usize * 4];
    for y in 0..height as usize {
        let y0 = y * sh / height as usize;
        let y1 = ((y + 1) * sh / height as usize).max(y0 + 1).min(sh);
        for x in 0..width as usize {
            let x0 = x * sw / width as usize;
            let x1 = ((x + 1) * sw / width as usize).max(x0 + 1).min(sw);
            let mut sum = [0u32; 4];
            for row in y0..y1 {
                let start = (row * sw + x0) * 4;
                for px in image.rgba[start..start + (x1 - x0) * 4].chunks_exact(4) {
                    for c in 0..4 {
                        sum[c] += u32::from(px[c]);
                    }
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            let out = (y * width as usize + x) * 4;
            for c in 0..4 {
                rgba[out + c] = (sum[c] / n) as u8;
            }
        }
    }
    Image {
        width,
        height,
        rgba,
    }
}

/// Whether the content band is one flat colour (a protected or empty screen).
pub fn content_band_is_blank(image: &Image) -> bool {
    band_is_flat(image.width as usize, image.height as usize, &image.rgba)
}

/// The same test on any 4-bytes-per-pixel buffer (RGBA screenshots, BGRA video
/// frames: channel order does not matter for "one flat colour").
pub fn band_is_flat(w: usize, h: usize, px: &[u8]) -> bool {
    if w == 0 || h == 0 || px.len() < w * h * 4 {
        return false;
    }
    let top = (h as f64 * BAND_TOP) as usize;
    let bottom = (h as f64 * BAND_BOTTOM) as usize;
    let step = (w.min(h) / 120).max(2);
    let pixel = |x: usize, y: usize| {
        let i = (y * w + x) * 4;
        [px[i] as i32, px[i + 1] as i32, px[i + 2] as i32]
    };
    // Dominant colour: the most common of the sampled colours, bucketed.
    let mut buckets: std::collections::HashMap<[i32; 3], usize> = Default::default();
    let mut samples = Vec::new();
    for y in (top..bottom).step_by(step) {
        for x in (0..w).step_by(step) {
            let p = pixel(x, y);
            *buckets
                .entry([p[0] / 16, p[1] / 16, p[2] / 16])
                .or_default() += 1;
            samples.push(p);
        }
    }
    let Some((bucket, _)) = buckets.into_iter().max_by_key(|(_, n)| *n) else {
        return false;
    };
    let centre = [bucket[0] * 16 + 8, bucket[1] * 16 + 8, bucket[2] * 16 + 8];
    let same = samples
        .iter()
        .filter(|p| (0..3).all(|c| (p[c] - centre[c]).abs() <= SAME_COLOUR + 8))
        .count();
    same as f64 >= samples.len() as f64 * BLANK_SHARE
}

/// Rows worth drawing: on screen, sized, inside the content band.
fn drawable_rows<'a>(
    rows: &'a [ElementRow],
    window: (f64, f64),
) -> impl Iterator<Item = &'a ElementRow> + 'a {
    let (ww, wh) = window;
    rows.iter().filter(move |row| {
        let [x, y, w, h] = row.rect;
        row.visible != Some(false)
            && [x, y, w, h].iter().all(|v| v.is_finite())
            && w >= 4.0
            && h >= 4.0
            && x < ww
            && y < wh * BAND_BOTTOM
            && y + h > wh * BAND_TOP
            && x + w > 0.0
            // The window/app containers span the whole screen: noise.
            && !(w >= ww * 0.98 && h >= wh * 0.9)
    })
}

/// A blank band over an accessibility tree with labelled content in it is a
/// protected screen, not an empty one.
pub fn tree_has_hidden_content(rows: &[ElementRow], window: (f64, f64)) -> bool {
    drawable_rows(rows, window)
        .filter(|row| !row.label.trim().is_empty())
        .count()
        >= MIN_LABELLED_ROWS
}

const FONT_PATHS: &[&str] = &[
    "/System/Library/Fonts/Hiragino Sans GB.ttc",
    "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc",
    "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
    "/Library/Fonts/Arial Unicode.ttf",
    "/System/Library/Fonts/Helvetica.ttc",
];

fn load_font() -> Option<ab_glyph::FontVec> {
    static FONT_BYTES: std::sync::OnceLock<Option<Vec<u8>>> = std::sync::OnceLock::new();
    let bytes = FONT_BYTES
        .get_or_init(|| FONT_PATHS.iter().find_map(|p| std::fs::read(p).ok()))
        .as_ref()?;
    ab_glyph::FontVec::try_from_vec_and_index(bytes.clone(), 0).ok()
}

/// Draw the tree over the blank band: per row an outline coloured by kind and
/// its label (or value), plus a banner naming what the picture is.
pub fn draw_wireframe(image: &mut Image, rows: &[ElementRow], window: (f64, f64)) {
    let scale = image.width as f64 / window.0.max(1.0);
    let font = load_font();
    let mut canvas = Canvas {
        image,
        font: font.as_ref(),
    };
    // Deeper rows last, so a button's label lands over its cell's outline.
    let mut drawable: Vec<&ElementRow> = drawable_rows(rows, window).collect();
    drawable.sort_by_key(|row| row.depth);
    for row in drawable {
        let [x, y, w, h] = row.rect.map(|v| v * scale);
        let colour = kind_colour(&row.kind);
        canvas.stroke_rect(x, y, w, h, colour, (scale * 0.6).max(1.0));
        let (text, colour) = if !row.label.trim().is_empty() {
            (row.label.trim().to_string(), colour)
        } else if let Some(value) = row.value.as_deref().filter(|v| !v.trim().is_empty()) {
            (value.trim().to_string(), colour)
        } else if matches!(
            row.kind.as_str(),
            "Image"
                | "Icon"
                | "TextField"
                | "SecureTextField"
                | "SearchField"
                | "TextView"
                | "Switch"
                | "Slider"
        ) {
            // No text to show: name the kind, faintly, so the box means something.
            (format!("[{}]", row.kind), [160, 160, 160, 255])
        } else {
            continue;
        };
        let size = (h * 0.55).clamp(10.0 * scale, 17.0 * scale);
        canvas.text(
            x + 3.0 * scale,
            y + (h - size) / 2.0,
            w - 6.0 * scale,
            size,
            &text,
            colour,
        );
    }
    let banner = "截图被 App 屏蔽 · 下面是按控件树画的线框图 / capture blocked by the app — wireframe from the accessibility tree";
    let band_top = image_band_top(canvas.image, window, scale);
    canvas.fill_rect(
        0.0,
        band_top,
        canvas.image.width as f64,
        22.0 * scale,
        [255, 236, 179, 255],
    );
    canvas.text(
        6.0 * scale,
        band_top + 4.0 * scale,
        canvas.image.width as f64 - 12.0 * scale,
        13.0 * scale,
        banner,
        [120, 70, 0, 255],
    );
}

fn image_band_top(image: &Image, window: (f64, f64), scale: f64) -> f64 {
    (window.1 * BAND_TOP * scale).min(image.height as f64)
}

fn kind_colour(kind: &str) -> [u8; 4] {
    match kind {
        "Button" | "Link" | "Cell" | "Switch" | "Tab" => [0, 102, 221, 255],
        "TextField" | "SecureTextField" | "SearchField" | "TextView" => [0, 140, 70, 255],
        "Image" | "Icon" => [150, 90, 200, 255],
        _ => [90, 90, 90, 255],
    }
}

struct Canvas<'a> {
    image: &'a mut Image,
    font: Option<&'a ab_glyph::FontVec>,
}

impl Canvas<'_> {
    fn blend(&mut self, x: i64, y: i64, colour: [u8; 4], coverage: f32) {
        let (w, h) = (self.image.width as i64, self.image.height as i64);
        if x < 0 || y < 0 || x >= w || y >= h || coverage <= 0.0 {
            return;
        }
        let i = ((y * w + x) * 4) as usize;
        let a = coverage.min(1.0) * colour[3] as f32 / 255.0;
        for (channel, &target) in self.image.rgba[i..i + 3].iter_mut().zip(&colour[..3]) {
            let old = *channel as f32;
            *channel = (old + (target as f32 - old) * a).round() as u8;
        }
        self.image.rgba[i + 3] = 255;
    }

    fn fill_rect(&mut self, x: f64, y: f64, w: f64, h: f64, colour: [u8; 4]) {
        for py in y.max(0.0) as i64..(y + h) as i64 {
            for px in x.max(0.0) as i64..(x + w) as i64 {
                self.blend(px, py, colour, 1.0);
            }
        }
    }

    fn stroke_rect(&mut self, x: f64, y: f64, w: f64, h: f64, colour: [u8; 4], width: f64) {
        self.fill_rect(x, y, w, width, colour);
        self.fill_rect(x, y + h - width, w, width, colour);
        self.fill_rect(x, y, width, h, colour);
        self.fill_rect(x + w - width, y, width, h, colour);
    }

    /// One line of text at `size` px, clipped to `max_width` with an ellipsis.
    fn text(&mut self, x: f64, y: f64, max_width: f64, size: f64, text: &str, colour: [u8; 4]) {
        use ab_glyph::{Font as _, ScaleFont as _};
        let Some(font) = self.font else {
            return;
        };
        let scaled = font.as_scaled(ab_glyph::PxScale::from(size as f32));
        let ascent = scaled.ascent();
        let ellipsis_width = scaled.h_advance(font.glyph_id('…'));
        let mut caret = x as f32;
        let limit = (x + max_width) as f32;
        let chars: Vec<char> = text.chars().filter(|c| !c.is_control()).collect();
        for (n, ch) in chars.iter().enumerate() {
            let id = font.glyph_id(*ch);
            let advance = scaled.h_advance(id);
            let last = n + 1 == chars.len();
            let (id, advance) = if !last && caret + advance + ellipsis_width > limit {
                (font.glyph_id('…'), ellipsis_width)
            } else {
                (id, advance)
            };
            if caret + advance > limit + 0.5 {
                break;
            }
            let glyph = id.with_scale_and_position(
                ab_glyph::PxScale::from(size as f32),
                ab_glyph::point(caret, y as f32 + ascent),
            );
            if let Some(outlined) = font.outline_glyph(glyph) {
                let bounds = outlined.px_bounds();
                outlined.draw(|gx, gy, coverage| {
                    self.blend(
                        bounds.min.x as i64 + gx as i64,
                        bounds.min.y as i64 + gy as i64,
                        colour,
                        coverage,
                    );
                });
            }
            caret += advance;
            if id == font.glyph_id('…') && !last {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_within_averages_and_never_enlarges() {
        // Two columns, black and white: halving the width averages them.
        let image = Image {
            width: 2,
            height: 2,
            rgba: vec![
                0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 255, 255, 255, 255, 255,
            ],
        };
        let small = fit_within(image, 1);
        assert_eq!((small.width, small.height), (1, 1));
        assert_eq!(small.rgba, vec![127, 127, 127, 255]);
        let big = fit_within(flat(10, 20, [1, 2, 3]), 40);
        assert_eq!((big.width, big.height), (10, 20));
    }

    fn flat(width: u32, height: u32, rgb: [u8; 3]) -> Image {
        Image {
            width,
            height,
            rgba: (0..width * height)
                .flat_map(|_| [rgb[0], rgb[1], rgb[2], 255])
                .collect(),
        }
    }

    fn row(kind: &str, label: &str, rect: [f64; 4]) -> ElementRow {
        ElementRow {
            kind: kind.to_string(),
            label: label.to_string(),
            rect,
            depth: 3,
            ..Default::default()
        }
    }

    #[test]
    fn a_flat_band_is_blank_even_with_bars_drawn_above_and_below() {
        let mut image = flat(390, 844, [255, 255, 255]);
        // status bar and tab bar content, outside the band
        for y in 0..40 {
            for x in 0..390 {
                image.rgba[(y * 390 + x) * 4] = 0;
            }
        }
        for y in 780..844 {
            for x in 0..390 {
                image.rgba[(y * 390 + x) * 4 + 1] = 0;
            }
        }
        assert!(content_band_is_blank(&image));
    }

    #[test]
    fn a_busy_band_is_not_blank() {
        let mut image = flat(390, 844, [255, 255, 255]);
        for y in 100..700 {
            for x in 0..390 {
                if (x / 20 + y / 20) % 2 == 0 {
                    let i = (y * 390 + x) * 4;
                    image.rgba[i..i + 3].copy_from_slice(&[20, 20, 20]);
                }
            }
        }
        assert!(!content_band_is_blank(&image));
    }

    #[test]
    fn hidden_content_needs_labelled_rows_inside_the_band() {
        let window = (390.0, 844.0);
        let rows = vec![
            row("Application", "PayPay", [0.0, 0.0, 390.0, 844.0]),
            row("StaticText", "残高", [20.0, 120.0, 100.0, 24.0]),
            row("Button", "支払う", [20.0, 200.0, 160.0, 44.0]),
        ];
        assert!(
            !tree_has_hidden_content(&rows, window),
            "two labelled rows is not enough"
        );
        let mut more = rows;
        more.push(row("Button", "チャージ", [200.0, 200.0, 160.0, 44.0]));
        assert!(tree_has_hidden_content(&more, window));
        // Rows only in the status/tab bars do not count.
        let bars = vec![
            row("Button", "a", [0.0, 5.0, 50.0, 20.0]),
            row("Button", "b", [60.0, 5.0, 50.0, 20.0]),
            row("Button", "c", [0.0, 800.0, 50.0, 40.0]),
        ];
        assert!(!tree_has_hidden_content(&bars, window));
    }

    #[test]
    fn the_wireframe_draws_outlines_and_survives_a_png_round_trip() {
        let window = (390.0, 844.0);
        let mut image = flat(1170, 2532, [255, 255, 255]);
        let rows = vec![
            row("Button", "支払う", [20.0, 200.0, 160.0, 44.0]),
            row("StaticText", "残高 1,234円", [20.0, 300.0, 300.0, 30.0]),
        ];
        draw_wireframe(&mut image, &rows, window);
        assert!(!content_band_is_blank(&image), "something was drawn");
        // The button's top-left outline pixel takes the button colour.
        let (x, y) = (60usize, 600usize); // (20,200) points × 3
        let i = (y * 1170 + x) * 4;
        assert_eq!(&image.rgba[i..i + 3], &kind_colour("Button")[..3]);
        let png = encode_png(&image).unwrap();
        let back = decode_png(&png).unwrap();
        assert_eq!((back.width, back.height), (1170, 2532));
    }
}

#[cfg(test)]
mod sample {
    use super::*;

    /// `WIREFRAME_SAMPLE=/path/out.png cargo test --lib redaction::sample -- --ignored`
    /// renders a PayPay-like screen to eyeball the drawing.
    #[test]
    #[ignore]
    fn render_sample() {
        let Ok(path) = std::env::var("WIREFRAME_SAMPLE") else {
            return;
        };
        let window = (440.0, 956.0);
        let mut image = Image {
            width: 1320,
            height: 2868,
            rgba: vec![255; 1320 * 2868 * 4],
        };
        let r = |kind: &str, label: &str, rect: [f64; 4], depth: u32| ElementRow {
            kind: kind.into(),
            label: label.into(),
            rect,
            depth,
            ..Default::default()
        };
        let rows = vec![
            r("Other", "", [16.0, 90.0, 408.0, 180.0], 2),
            r("StaticText", "PayPay残高", [32.0, 110.0, 200.0, 24.0], 3),
            r("StaticText", "12,345円", [32.0, 140.0, 240.0, 44.0], 3),
            r("Button", "チャージ", [32.0, 210.0, 120.0, 44.0], 3),
            r("Button", "送る・受け取る", [170.0, 210.0, 160.0, 44.0], 3),
            r("Image", "", [140.0, 320.0, 160.0, 160.0], 3),
            r("Button", "スキャン支払い", [32.0, 500.0, 180.0, 56.0], 3),
            r("Button", "支払う", [228.0, 500.0, 180.0, 56.0], 3),
            r(
                "Cell",
                "取引履歴 すべての取引を見る",
                [16.0, 590.0, 408.0, 60.0],
                3,
            ),
            r("TextField", "", [16.0, 670.0, 408.0, 44.0], 3),
        ];
        draw_wireframe(&mut image, &rows, window);
        std::fs::write(path, encode_png(&image).unwrap()).unwrap();
    }
}
