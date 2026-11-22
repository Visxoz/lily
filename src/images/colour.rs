//! Decodes artwork at the size it is drawn, and picks the colours drawn
//! behind it and in it.

use slint::{Color, Rgba8Pixel, SharedPixelBuffer};

use super::{BANNER, HEADER, PLAYER};

type Pixels = SharedPixelBuffer<Rgba8Pixel>;
/// The artwork, and its accent and backdrop where `edge` needs them.
pub(super) type Decoded = (Pixels, Option<Color>, Option<Pixels>);

/// Decodes artwork at `pixels`, drawn at `edge` logical pixels.
pub(super) fn decode(bytes: &[u8], pixels: u32, edge: u32) -> Option<Decoded> {
    let image = image::load_from_memory(bytes)
        .map_err(|e| log::debug!("undecodable artwork: {e}"))
        .ok()?;
    // Scale so the shorter side is `pixels`; the UI crops to fill.
    let (w, h) = (image.width(), image.height());
    let scale = pixels as f32 / w.min(h).max(1) as f32;
    let image = if scale < 1.0 {
        image.resize(
            ((w as f32 * scale).round() as u32).max(1),
            ((h as f32 * scale).round() as u32).max(1),
            image::imageops::FilterType::CatmullRom,
        )
    } else {
        image
    };
    // Only artwork with something drawn behind it needs a colour picked.
    let small = matches!(edge, PLAYER | HEADER | BANNER).then(|| sample(&image));
    let accent = small.as_ref().map(accent_of);
    let backdrop = match edge {
        PLAYER => small.as_ref().map(backdrop_of),
        HEADER => Some(blurred(&image)),
        _ => None,
    };
    // Consumes the image, so one already in RGBA is not copied first.
    let rgba = image.into_rgba8();
    let buffer = SharedPixelBuffer::clone_from_slice(rgba.as_raw(), rgba.width(), rgba.height());
    Some((buffer, accent, backdrop))
}

const SAMPLE: u32 = 16;
const MESH: usize = 4;
/// Enough pixels that the GPU's bilinear stretch shows no creases.
const BACKDROP: usize = 24;

fn sample(image: &image::DynamicImage) -> image::RgbImage {
    image
        .resize_exact(SAMPLE, SAMPLE, image::imageops::FilterType::Triangle)
        .to_rgb8()
}

/// The artwork's strongest colour, darkened enough for white text, as
/// Spotify tints its page headers.
fn accent_of(small: &image::RgbImage) -> Color {
    // Pixels are pooled by hue, so artwork of several colours gives its
    // strongest one rather than their average, which tends to grey.
    const HUES: usize = 12;
    let mut pools = [[0.0f32; 5]; HUES];
    for p in small.pixels() {
        let [pr, pg, pb] = p.0.map(|c| c as f32 / 255.0);
        let (h, s, l) = rgb_to_hsl(pr, pg, pb);
        // Saturated mid-tones count; greys, black and white hardly do.
        let chroma = s * (1.0 - (2.0 * l - 1.0).abs());
        // Cubed, so a vivid area outweighs a larger dull one: a sky over
        // beige buildings is blue, not beige.
        let weight = chroma.powi(3);
        // Offset half a pool, so red, which sits at 0° and 360°, and the
        // other primaries fall in the middle of one rather than across two.
        let pool = &mut pools[((h / 360.0 * HUES as f32) + 0.5) as usize % HUES];
        for (sum, c) in pool
            .iter_mut()
            .zip([pr * weight, pg * weight, pb * weight, weight, chroma])
        {
            *sum += c;
        }
    }
    let [r, g, b, total, colour] = pools
        .into_iter()
        .max_by(|a, b| a[3].total_cmp(&b[3]))
        .unwrap_or_default();
    let (r, g, b) = if total > 0.0 {
        (r / total, g / total, b / total)
    } else {
        (0.0, 0.0, 0.0)
    };
    let (h, s, _) = rgb_to_hsl(r, g, b);
    // Muted artwork with a hue all the same, such as a bluish photo, stays
    // clear of greyish artwork's floor, so `vivid` can tell them apart.
    // Chroma, unlike saturation, stays low for near-black with a cast.
    let chroma = r.max(g).max(b) - r.min(g).min(b);
    let s = if chroma >= 0.05 && colour >= COLOURFUL_WEIGHT {
        s.clamp(MIN_ACCENT_SATURATION + 0.1, 0.7)
    } else {
        MIN_ACCENT_SATURATION
    };
    // Bright hues such as yellow are darkened further to keep white text
    // at 4.5:1.
    readable(from_hsl(h, s, 0.32), true)
}

/// The artwork's colours where they sit in it, as a blurred copy would show
/// them without the cost of blurring: a grid of them, each readable behind
/// white text and each row darker, blended smoothly.
fn backdrop_of(small: &image::RgbImage) -> Pixels {
    let block = SAMPLE as usize / MESH;
    let mesh: [[f32; 3]; MESH * MESH] = std::array::from_fn(|cell| {
        let (row, column) = (cell / MESH, cell % MESH);
        let mut sum = [0.0f32; 3];
        for y in row * block..(row + 1) * block {
            for x in column * block..(column + 1) * block {
                let pixel = small.get_pixel(x as u32, y as u32).0;
                for (total, c) in sum.iter_mut().zip(pixel) {
                    *total += f32::from(c) / 255.0;
                }
            }
        }
        let [r, g, b] = sum.map(|c| c / (block * block) as f32);
        let (h, _, l) = rgb_to_hsl(r, g, b);
        // Chroma rather than saturation, which runs high for near-black
        // with a cast.
        let chroma = r.max(g).max(b) - r.min(g).min(b);
        let (r, g, b) = channels(readable(
            from_hsl(h, (chroma * 1.5).min(0.8), 0.25 + l * 0.1),
            true,
        ));
        let dim = 1.0 + row as f32 / (2 * (MESH - 1)) as f32;
        [r, g, b].map(|c| c / dim)
    });
    // Each grid row's or column's weight along an axis: a cubic B-spline,
    // never negative, so no pixel is brighter than the cells it mixes.
    let axis: [[f32; MESH]; BACKDROP] = std::array::from_fn(|v| {
        let u = v as f32 / (BACKDROP - 1) as f32 * (MESH - 1) as f32;
        let mut weights = [0.0; MESH];
        for k in -1..=MESH as i32 {
            let x = (u - k as f32).abs();
            let weight = if x < 1.0 {
                (4.0 - 6.0 * x * x + 3.0 * x * x * x) / 6.0
            } else {
                (2.0 - x).max(0.0).powi(3) / 6.0
            };
            weights[k.clamp(0, MESH as i32 - 1) as usize] += weight;
        }
        weights
    });
    let mut pixels = Pixels::new(BACKDROP as u32, BACKDROP as u32);
    for (i, pixel) in pixels.make_mut_slice().iter_mut().enumerate() {
        let (rows, columns) = (axis[i / BACKDROP], axis[i % BACKDROP]);
        let mut mixed = [0.0f32; 3];
        for (cell, colour) in mesh.iter().enumerate() {
            let weight = rows[cell / MESH] * columns[cell % MESH];
            for (sum, c) in mixed.iter_mut().zip(colour) {
                *sum += weight * c;
            }
        }
        let [r, g, b] = mixed;
        let byte = |c: f32| (c * 255.0).round() as u8;
        *pixel = Rgba8Pixel::new(byte(r), byte(g), byte(b), 255);
    }
    pixels
}

/// A header's artwork, shrunk and blurred: cheap while small, and the GPU's
/// stretch smooths it further.
fn blurred(image: &image::DynamicImage) -> Pixels {
    let small = image
        .resize_to_fill(BLURRED.0, BLURRED.1, image::imageops::FilterType::Triangle)
        .blur(BLUR_SIGMA)
        .into_rgba8();
    Pixels::clone_from_slice(small.as_raw(), small.width(), small.height())
}

/// A wide strip, about a page header's shape.
const BLURRED: (u32, u32) = (192, 64);
/// About 40 logical pixels across a page.
const BLUR_SIGMA: f32 = 6.0;

/// The least saturation `accent_of` gives, which greyish artwork gets.
const MIN_ACCENT_SATURATION: f32 = 0.25;
/// How much colour, summed over `accent_of`'s 256 pixels, makes a hue
/// worth following: a few strong pixels, or a faint wash over a tenth.
const COLOURFUL_WEIGHT: f32 = 2.0;

/// A saturated take on an `accent`, for controls drawn in it rather than
/// backgrounds behind white text, or `None` for greyish artwork, whose hue
/// means nothing.
pub fn vivid(accent: Color) -> Option<Color> {
    let (r, g, b) = channels(accent);
    let (h, s, _) = rgb_to_hsl(r, g, b);
    // A little above the floor, as the colour went through 8-bit channels.
    (s > MIN_ACCENT_SATURATION + 0.05).then(|| from_hsl(h, s.max(0.5), 0.5))
}

/// `colour` made just light or dark enough to read: as text on the light
/// look's white, or, in the dark looks, as text on black and behind white
/// icons. WCAG asks 4.5:1 of text and 3:1 of icons.
pub fn readable(colour: Color, light: bool) -> Color {
    let (lowest, highest) = if light { (0.0, 0.18) } else { (0.175, 0.3) };
    let (r, g, b) = channels(colour);
    let lum = luminance(r, g, b);
    if (lowest..=highest).contains(&lum) {
        return colour;
    }
    // A little inside the range, which rounding to 8-bit channels could
    // otherwise step out of.
    let target = if lum > highest {
        highest - 0.01
    } else {
        lowest + 0.01
    };
    // Luminance rises with lightness, so halve the range towards the target.
    let (h, s, _) = rgb_to_hsl(r, g, b);
    let (mut low, mut high) = (0.0f32, 1.0f32);
    for _ in 0..20 {
        let mid = f32::midpoint(low, high);
        let (r, g, b) = hsl_to_rgb(h, s, mid);
        if luminance(r, g, b) < target {
            low = mid;
        } else {
            high = mid;
        }
    }
    from_hsl(h, s, f32::midpoint(low, high))
}

/// WCAG's relative luminance.
fn luminance(r: f32, g: f32, b: f32) -> f32 {
    let linear = |c: f32| {
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
}

fn channels(colour: Color) -> (f32, f32, f32) {
    let [r, g, b] = [colour.red(), colour.green(), colour.blue()].map(|c| f32::from(c) / 255.0);
    (r, g, b)
}

fn from_hsl(h: f32, s: f32, l: f32) -> Color {
    let (r, g, b) = hsl_to_rgb(h, s, l);
    let byte = |c: f32| (c * 255.0).round() as u8;
    Color::from_rgb_u8(byte(r), byte(g), byte(b))
}

fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = f32::midpoint(max, min);
    let d = max - min;
    if d < 1e-6 {
        return (0.0, 0.0, l);
    }
    let s = d / (1.0 - (2.0 * l - 1.0).abs());
    let h = if max == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h * 60.0, s, l)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0).rem_euclid(2.0) - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    (r + m, g + m, b + m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_hsl() {
        for (r, g, b) in [
            (1.0, 0.0, 0.0),
            (0.2, 0.6, 0.4),
            (0.5, 0.5, 0.5),
            (0.1, 0.2, 0.9),
        ] {
            let (h, s, l) = rgb_to_hsl(r, g, b);
            let (r2, g2, b2) = hsl_to_rgb(h, s, l);
            assert!((r - r2).abs() < 1e-4 && (g - g2).abs() < 1e-4 && (b - b2).abs() < 1e-4);
        }
    }

    #[test]
    fn accent_keeps_the_hue_and_darkens() {
        let red = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            8,
            8,
            image::Rgb([230, 20, 20]),
        ));
        let accent = accent_of(&sample(&red));
        assert!(accent.red() > accent.green() && accent.red() > accent.blue());
        assert!(accent.red() < 180, "dark enough for white text");

        let bright = vivid(accent).expect("red artwork has a hue");
        assert!(bright.red() > accent.red(), "brighter");
        assert!(
            bright.red() > bright.green() && bright.red() > bright.blue(),
            "same hue"
        );

        let grey = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            8,
            8,
            image::Rgb([128, 128, 128]),
        ));
        assert_eq!(vivid(accent_of(&sample(&grey))), None, "no hue to follow");
    }

    #[test]
    fn follows_muted_and_mixed_artwork() {
        let muted = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            8,
            8,
            image::Rgb([90, 100, 130]),
        ));
        let blue = vivid(accent_of(&sample(&muted))).expect("a bluish photo has a hue");
        assert!(blue.blue() > blue.red() && blue.blue() > blue.green());

        // Red and cyan halves, which average to grey.
        let halves = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(8, 8, |x, _| {
            if x < 4 {
                image::Rgb([230, 20, 20])
            } else {
                image::Rgb([20, 230, 230])
            }
        }));
        let (r, g, b) =
            channels(vivid(accent_of(&sample(&halves))).expect("one of the two colours"));
        let (h, _, _) = rgb_to_hsl(r, g, b);
        assert!(
            !(15.0..165.0).contains(&h) && !(195.0..345.0).contains(&h),
            "red or cyan"
        );

        // A quarter of sky over beige buildings, the dull area the larger.
        let sky = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(8, 8, |_, y| {
            if y < 2 {
                image::Rgb([90, 150, 220])
            } else {
                image::Rgb([200, 170, 130])
            }
        }));
        let blue = vivid(accent_of(&sample(&sky))).expect("the sky's colour");
        assert!(blue.blue() > blue.red(), "the sky, not the beige");

        let near_black = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            8,
            8,
            image::Rgb([40, 36, 32]),
        ));
        assert_eq!(
            vivid(accent_of(&sample(&near_black))),
            None,
            "a cast isn't a colour"
        );

        let yellow = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            8,
            8,
            image::Rgb([240, 220, 20]),
        ));
        let (r, g, b) = channels(accent_of(&sample(&yellow)));
        assert!(luminance(r, g, b) <= 0.18, "white text still reads on it");
    }

    #[test]
    fn makes_accents_readable_in_each_look() {
        let lum = |c: Color| {
            let (r, g, b) = channels(c);
            luminance(r, g, b)
        };
        let green = Color::from_rgb_u8(0x1d, 0xb9, 0x54);
        assert!(lum(readable(green, true)) <= 0.18, "darkened on white");
        let red = Color::from_rgb_u8(0xff, 0x00, 0x33);
        assert_eq!(readable(red, false), red, "already fine on black");
        let blue = Color::from_rgb_u8(0, 0, 0xff);
        assert!(lum(readable(blue, false)) >= 0.175, "lightened on black");
        let yellow = Color::from_rgb_u8(0xee, 0xee, 0x22);
        let fixed = readable(yellow, false);
        assert!(lum(fixed) <= 0.3, "dark enough behind white icons");
        assert!(fixed.red() > fixed.blue(), "keeps its hue");
    }

    #[test]
    fn blurs_header_artwork_into_a_strip() {
        // Red above and below a blue middle third.
        let square = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(240, 240, |_, y| {
            image::Rgb(if (80..160).contains(&y) {
                [0, 0, 255]
            } else {
                [255, 0, 0]
            })
        }));
        let pixels = blurred(&square);
        assert_eq!((pixels.width(), pixels.height()), BLURRED);
        let middle = pixels.as_slice()[(BLURRED.1 / 2 * BLURRED.0) as usize];
        assert!(
            middle.b > middle.r,
            "the blue middle third stays in the middle"
        );
        let red = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            240,
            240,
            image::Rgb([200, 0, 0]),
        ));
        let corner = blurred(&red).as_slice()[0];
        assert!(
            corner.r >= 195 && corner.a == 255,
            "edges don't darken: {corner:?}"
        );
    }

    #[test]
    fn backdrop_keeps_colours_where_they_sit() {
        let halves = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(8, 8, |x, _| {
            image::Rgb(if x < 4 { [240, 220, 20] } else { [20, 60, 220] })
        }));
        let pixels = backdrop_of(&sample(&halves));
        let at = |x: usize, y: usize| {
            let p = pixels.as_slice()[y * BACKDROP + x];
            (
                f32::from(p.r) / 255.0,
                f32::from(p.g) / 255.0,
                f32::from(p.b) / 255.0,
            )
        };
        let (r, _, b) = at(0, 0);
        assert!(r > b, "yellow at the left");
        let (r, _, b) = at(BACKDROP - 1, 0);
        assert!(b > r, "blue at the right");
        for p in pixels.as_slice() {
            let [r, g, b] = [p.r, p.g, p.b].map(|c| f32::from(c) / 255.0);
            assert!(luminance(r, g, b) <= 0.18, "white text reads on it");
        }
        let lum = |(r, g, b)| luminance(r, g, b);
        assert!(
            lum(at(0, BACKDROP - 1)) < lum(at(0, 0)),
            "darker at the bottom"
        );
    }
}
