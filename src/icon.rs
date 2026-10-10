//! Иконка программы: перекрашенная иконка AmneziaWG (PNG из assets/icon, по одному на размер).
//! Общая для окна (заголовок, панель задач, «О программе») и трея; в exe её вшивает build.rs.

/// Цвет точки состояния в трее (RGB).
pub type Dot = [u8; 3];

pub const GRAY: Dot = [150, 156, 163];
pub const GREEN: Dot = [64, 200, 100];
pub const YELLOW: Dot = [245, 195, 50];
pub const RED: Dot = [235, 72, 66];

/// Тёмная обводка точки, чтобы она читалась на любой панели задач.
const OUTLINE: Dot = [16, 16, 20];

macro_rules! icon {
    ($size:literal) => {
        ($size, include_bytes!(concat!("../assets/icon/icon-", $size, ".png")).as_slice())
    };
}

/// Готовые размеры по возрастанию: (сторона, PNG).
const IMAGES: [(u32, &[u8]); 10] =
    [icon!(16), icon!(20), icon!(24), icon!(32), icon!(40), icon!(48), icon!(64), icon!(96), icon!(128), icon!(256)];

/// RGBA-пиксели иконки `size`×`size`: готовый размер, если есть, иначе ближайший больший,
/// уменьшенный усреднением по площади (больше 256 — растягивается самый большой).
pub fn rgba(size: u32) -> Vec<u8> {
    let &(src_size, png_data) = IMAGES.iter().find(|(s, _)| *s >= size).unwrap_or(&IMAGES[IMAGES.len() - 1]);
    let src = decode(png_data);
    if src_size == size {
        src
    } else {
        resample(&src, src_size, size)
    }
}

/// Иконка в цвете режима: обычная (режим 1) или жёлтая (режим 2, встроенный движок) — режим виден в панели
/// задач, трее и заголовке.
pub fn themed(size: u32, engine: bool) -> Vec<u8> {
    let mut px = rgba(size);
    if engine {
        retint(&mut px, ENGINE_HUE);
    }
    px
}

/// Иконка с точкой состояния в правом нижнем углу — значок в трее и в панели задач.
pub fn rgba_with_dot(size: u32, engine: bool, color: Dot) -> Vec<u8> {
    let mut px = themed(size, engine);
    draw_dot(&mut px, size, color);
    px
}

fn decode(data: &[u8]) -> Vec<u8> {
    let mut reader = png::Decoder::new(data).read_info().expect("PNG иконки");
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).expect("кадр PNG иконки");
    assert!(info.color_type == png::ColorType::Rgba && info.bit_depth == png::BitDepth::Eight, "иконка должна быть RGBA 8 бит");
    buf.truncate(info.buffer_size());
    buf
}

/// Масштабирование усреднением по площади (с предумножением на альфу, чтобы края не темнели).
fn resample(src: &[u8], from: u32, to: u32) -> Vec<u8> {
    let (from, to) = (from as usize, to as usize);
    let span = |i: usize| {
        let lo = i * from / to;
        (lo, ((i + 1) * from).div_ceil(to).max(lo + 1).min(from))
    };
    let mut out = Vec::with_capacity(to * to * 4);
    for y in 0..to {
        let (y0, y1) = span(y);
        for x in 0..to {
            let (x0, x1) = span(x);
            let mut acc = [0u32; 4]; // r*a, g*a, b*a, a
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let p = &src[(sy * from + sx) * 4..][..4];
                    let a = p[3] as u32;
                    acc[0] += p[0] as u32 * a;
                    acc[1] += p[1] as u32 * a;
                    acc[2] += p[2] as u32 * a;
                    acc[3] += a;
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            for c in &acc[..3] {
                out.push(((*c + acc[3] / 2).checked_div(acc[3]).unwrap_or(0)) as u8);
            }
            out.push(((acc[3] + n / 2) / n) as u8);
        }
    }
    out
}

/// Оттенок иконки в режиме 2: ярко-жёлтый.
const ENGINE_HUE: f32 = 50.0;

/// Перекрасить цветные пиксели в оттенок `hue` (градусы), сохранив насыщенность и яркость; серые и тёмные
/// (фон, обводка) не меняются.
fn retint(px: &mut [u8], hue: f32) {
    for p in px.as_chunks_mut::<4>().0 {
        let [r, g, b] = [p[0], p[1], p[2]].map(|v| v as f32 / 255.0);
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        if max <= 0.0 || (max - min) / max < 0.2 {
            continue;
        }
        let c = max - min;
        let h = hue / 60.0;
        let x = c * (1.0 - (h % 2.0 - 1.0).abs());
        let (r1, g1, b1) = match h as u32 {
            0 => (c, x, 0.0),
            1 => (x, c, 0.0),
            2 => (0.0, c, x),
            3 => (0.0, x, c),
            4 => (x, 0.0, c),
            _ => (c, 0.0, x),
        };
        for (i, v) in [r1, g1, b1].into_iter().enumerate() {
            p[i] = ((v + min) * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }
}

/// Одна точка состояния во весь квадрат `size`×`size` на прозрачном фоне — значок поверх кнопки окна в панели
/// задач (overlay).
pub fn dot_icon(size: u32, color: Dot) -> Vec<u8> {
    let mut px = vec![0; (size * size * 4) as usize];
    draw_circle(&mut px, size, size, color);
    px
}

/// Кружок с обводкой в правом нижнем углу; меняет только пиксели внутри квадрата кружка.
fn draw_dot(px: &mut [u8], size: u32, color: Dot) {
    let diameter = ((size as f32 * 0.46).round() as u32).max(6).min(size);
    draw_circle(px, size, diameter, color);
}

/// Кружок диаметра `diameter` с тёмной обводкой, вписанный в правый нижний угол картинки `size`×`size`.
fn draw_circle(px: &mut [u8], size: u32, diameter: u32, color: Dot) {
    let r = diameter as f32 / 2.0;
    let c = size as f32 - r; // центр кружка
    let first = (size - diameter) as usize;
    for y in first..size as usize {
        for x in first..size as usize {
            let d = ((x as f32 + 0.5 - c).powi(2) + (y as f32 + 0.5 - c).powi(2)).sqrt();
            let p = &mut px[(y * size as usize + x) * 4..][..4];
            blend(p, OUTLINE, (r - d + 0.5).clamp(0.0, 1.0));
            blend(p, color, (r - 1.0 - d + 0.5).clamp(0.0, 1.0));
        }
    }
}

/// Накладывает цвет с покрытием `cover` на обычный (непредумноженный) RGBA-пиксель.
fn blend(p: &mut [u8], rgb: Dot, cover: f32) {
    if cover <= 0.0 {
        return;
    }
    let a = p[3] as f32 / 255.0;
    let out_a = cover + a * (1.0 - cover);
    for i in 0..3 {
        let v = (rgb[i] as f32 * cover + p[i] as f32 * a * (1.0 - cover)) / out_a;
        p[i] = v.round().clamp(0.0, 255.0) as u8;
    }
    p[3] = (out_a * 255.0).round() as u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba_has_exact_length_for_any_size() {
        for size in [16, 20, 24, 32, 40, 48, 64, 100, 128, 200, 256, 300] {
            assert_eq!(rgba(size).len(), (size * size * 4) as usize, "размер {size}");
        }
    }

    #[test]
    fn ready_size_is_returned_as_is() {
        assert_eq!(rgba(32), decode(IMAGES[3].1));
    }

    #[test]
    fn downscale_keeps_transparent_corner_and_opaque_center() {
        let px = rgba(100);
        assert_eq!(px[3], 0, "угол прозрачный");
        assert!(px[(50 * 100 + 50) * 4 + 3] > 200, "центр непрозрачный");
    }

    #[test]
    fn dot_changes_only_bottom_right_corner() {
        for size in [16, 24, 32] {
            let base = rgba(size);
            let with = rgba_with_dot(size, false, GREEN);
            let diameter = ((size as f32 * 0.46).round() as u32).max(6);
            let first = size - diameter;
            let mut changed = 0;
            for y in 0..size {
                for x in 0..size {
                    let i = ((y * size + x) * 4) as usize;
                    if base[i..i + 4] != with[i..i + 4] {
                        changed += 1;
                        assert!(x >= first && y >= first, "{size}: лишний пиксель ({x}, {y})");
                    }
                }
            }
            assert!(changed > 0, "{size}: точка не нарисована");
            let mid = ((size - diameter / 2 - 1) * size + (size - diameter / 2 - 1)) as usize * 4;
            assert_eq!(&with[mid..mid + 4], &[GREEN[0], GREEN[1], GREEN[2], 255], "{size}: центр точки");
        }
    }

    #[test]
    fn dot_icon_is_colored_center_with_transparent_corners() {
        for size in [16, 20, 24, 32] {
            let px = dot_icon(size, RED);
            assert_eq!(px.len(), (size * size * 4) as usize, "{size}: длина");
            let at = |x: u32, y: u32| &px[((y * size + x) * 4) as usize..][..4];
            assert_eq!(at(size / 2, size / 2), &[RED[0], RED[1], RED[2], 255], "{size}: центр — цвет состояния");
            for (x, y) in [(0, 0), (size - 1, 0), (0, size - 1), (size - 1, size - 1)] {
                assert_eq!(at(x, y)[3], 0, "{size}: угол ({x}, {y}) прозрачный");
            }
            let edge = at(size / 2, 0);
            assert!(edge[3] > 0 && edge[..3] != RED[..], "{size}: у края — тёмная обводка, а не заливка: {edge:?}");
        }
    }

    #[test]
    fn dot_colors_differ() {
        let all = [GRAY, GREEN, YELLOW, RED].map(|c| rgba_with_dot(16, false, c));
        for i in 0..4 {
            for j in i + 1..4 {
                assert_ne!(all[i], all[j]);
            }
        }
    }

    #[test]
    fn engine_icon_is_yellow_and_keeps_shape() {
        let (base, yellow) = (rgba(64), themed(64, true));
        assert_eq!(themed(64, false), base, "режим 1 — иконка без изменений");
        let mut tinted = 0;
        for (a, b) in base.as_chunks::<4>().0.iter().zip(yellow.as_chunks::<4>().0.iter()) {
            assert_eq!(a[3], b[3], "прозрачность не меняется");
            if a != b {
                tinted += 1;
                assert!(b[0] >= b[2] && b[1] >= b[2], "перекрашенный пиксель жёлтый: {b:?}");
            }
        }
        assert!(tinted > 100, "розовые пиксели перекрашены: {tinted}");
    }
}
