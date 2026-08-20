//! 画素レベルの決定論的オペレーション(回転・クロップ・リサイズ・色調整)。
//!
//! パイプラインの中間表現は常に `image::RgbaImage`(RGBA8, 非プリマルチプライ)。
//! 浮動小数演算は f32/f64 の固定手順のみを使い、スレッド数や実行時刻に依存しない
//! (rayon による行分割は画素ごとに独立なため結果はスレッド数に依存しない)。

use fast_image_resize::images::{Image as FirImage, ImageRef};
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use image::{Rgba, RgbaImage};
use imageproc::geometric_transformations::{
    rotate_about_center, rotate_about_center_no_crop, Interpolation,
};

use crate::recipe::{Anchor, CropMode, Fit, Rect, RotateCrop};
use crate::transform::Affine;

/// op ヘルパの失敗。呼び出し側(engine)が op index / op 名を付けて
/// `AtxError::Operation` に包み直す。
type OpResult<T> = std::result::Result<T, String>;

/// EXIF Orientation(1-8)を画素へ焼き込む。1 / 未知の値は無変換。
pub(crate) fn apply_orientation(img: RgbaImage, orientation: u16) -> RgbaImage {
    use image::imageops::{flip_horizontal, flip_vertical, rotate180, rotate270, rotate90};
    match orientation {
        2 => flip_horizontal(&img),
        3 => rotate180(&img),
        4 => flip_vertical(&img),
        5 => rotate90(&flip_horizontal(&img)),
        6 => rotate90(&img),
        7 => rotate270(&flip_horizontal(&img)),
        8 => rotate270(&img),
        _ => img,
    }
}

/// Orientation を適用したときの実効寸法。
pub(crate) fn oriented_dimensions(width: u32, height: u32, orientation: u16) -> (u32, u32) {
    match orientation {
        5..=8 => (height, width),
        _ => (width, height),
    }
}

/// `apply_orientation` が座標に対して行う写像(連続座標)。
///
/// `w`/`h` は **適用前** の寸法。90 度系(5..=8)では出力寸法が入れ替わる。
/// 各式は `apply_orientation` の実装(flip / rotate90 / rotate270 の合成)から
/// 直接導いたもの:
/// `flip_h: (w-x, y)` / `flip_v: (x, h-y)` / `rot90: (h-y, x)` / `rot270: (y, w-x)`。
pub(crate) fn orientation_affine(width: u32, height: u32, orientation: u16) -> Affine {
    let w = width as f64;
    let h = height as f64;
    match orientation {
        // flip_horizontal
        2 => Affine::linear(-1.0, 0.0, w, 0.0, 1.0, 0.0),
        // rotate180
        3 => Affine::linear(-1.0, 0.0, w, 0.0, -1.0, h),
        // flip_vertical
        4 => Affine::linear(1.0, 0.0, 0.0, 0.0, -1.0, h),
        // rotate90(flip_horizontal) => (h - y, w - x)
        5 => Affine::linear(0.0, -1.0, h, -1.0, 0.0, w),
        // rotate90 => (h - y, x)
        6 => Affine::linear(0.0, -1.0, h, 1.0, 0.0, 0.0),
        // rotate270(flip_horizontal) => (y, x)(転置)
        7 => Affine::linear(0.0, 1.0, 0.0, 1.0, 0.0, 0.0),
        // rotate270 => (y, w - x)
        8 => Affine::linear(0.0, 1.0, 0.0, -1.0, 0.0, w),
        _ => Affine::IDENTITY,
    }
}

/// 任意角度回転。正の角度 = 時計回り。
///
/// - `crop = Full`: 回転後の外接矩形をキャンバスとし、余白は `pad` 色で塗る
/// - `crop = LargestInscribedRect`: 回転後の最大内接矩形(軸並行)で中央クロップ
///
/// 戻り値の第2要素は警告(内接矩形クロップで失われた画素の割合)、
/// 第3要素は「入力座標 → 出力座標」のアフィン変換(`coordinate_space: source` 用)。
///
/// # 変換の導出
///
/// - 90 の倍数: `image::imageops` の厳密回転。中心 `(w/2, h/2)` → `(ow/2, oh/2)` の
///   厳密な四半回転(連続座標)。
/// - 任意角: `imageproc` の `warp` 系は **index 座標** で中心を `(w/2, h/2)` と置くため、
///   連続座標へ換算した中心 `(w/2 + 0.5, h/2 + 0.5)` を使う
///   (index `p` と連続 `u` は `u = p + 0.5`。`u' = A(u - (c+0.5)) + (c_out + 0.5)`)。
/// - `LargestInscribedRect`: 上の回転に続けて内接矩形の左上 `(x, y)` ぶんの
///   **負の平行移動**を合成する(`x = (w - rw) / 2`, `y = (h - rh) / 2`、整数除算)。
/// - `Full`: 出力キャンバスが `ceil(h|sin| + w|cos|) x ceil(h|cos| + w|sin|)` に広がり、
///   `imageproc` は出力中心を `(ow/2, oh/2)` に置く。この中心ずれが実質的な
///   正の平行移動になる。
pub(crate) fn rotate(
    img: &RgbaImage,
    angle_degrees: f64,
    crop: RotateCrop,
    pad: [u8; 4],
) -> (RgbaImage, Option<String>, Affine) {
    let (w, h) = img.dimensions();
    // 90 の倍数は補間を挟まない厳密な回転で処理する(劣化・端の丸め誤差を避ける)。
    // 内接矩形も全体キャンバスも一致するため crop の区別は不要。
    if angle_degrees % 90.0 == 0.0 {
        let quarter = (angle_degrees / 90.0).rem_euclid(4.0) as u32;
        let out = match quarter {
            1 => image::imageops::rotate90(img),
            2 => image::imageops::rotate180(img),
            3 => image::imageops::rotate270(img),
            _ => img.clone(),
        };
        let (ow, oh) = out.dimensions();
        let xf = Affine::rotate_about(
            (quarter as f64) * std::f64::consts::FRAC_PI_2,
            (w as f64 / 2.0, h as f64 / 2.0),
            (ow as f64 / 2.0, oh as f64 / 2.0),
        );
        return (out, None, xf);
    }
    let theta = (angle_degrees as f32).to_radians();
    let border = imageproc::geometric_transformations::Border::Constant(Rgba(pad));

    match crop {
        RotateCrop::Full => {
            let out = rotate_about_center_no_crop(img, theta, Interpolation::Bicubic, border);
            let (ow, oh) = out.dimensions();
            let xf = Affine::rotate_about(
                theta as f64,
                (w as f64 / 2.0 + 0.5, h as f64 / 2.0 + 0.5),
                (ow as f64 / 2.0 + 0.5, oh as f64 / 2.0 + 0.5),
            );
            (out, None, xf)
        }
        RotateCrop::LargestInscribedRect => {
            // 元画像と同じキャンバスサイズで回転させ、その中の最大内接矩形を切り出す。
            let rotated = rotate_about_center(img, theta, Interpolation::Bicubic, border);
            let (rw, rh) = largest_inscribed_rect(w as f64, h as f64, angle_degrees.to_radians());
            let rw = (rw.floor() as u32).clamp(1, w);
            let rh = (rh.floor() as u32).clamp(1, h);
            let x = (w - rw) / 2;
            let y = (h - rh) / 2;
            let cropped = image::imageops::crop_imm(&rotated, x, y, rw, rh).to_image();
            let kept = (rw as f64 * rh as f64) / (w as f64 * h as f64);
            let removed_pct = (1.0 - kept) * 100.0;
            let warning = format!(
                "rotation crop (largest_inscribed_rect) removed {removed_pct:.1}% of pixels ({w}x{h} -> {rw}x{rh})"
            );
            let center = (w as f64 / 2.0 + 0.5, h as f64 / 2.0 + 0.5);
            let xf = Affine::rotate_about(theta as f64, center, center)
                .then(Affine::translate(-(x as f64), -(y as f64)));
            (cropped, Some(warning), xf)
        }
    }
}

/// w×h の矩形を angle ラジアン回転させたとき、その内部に収まる
/// 最大面積の軸並行矩形の寸法を返す(標準的な rotatedRectWithMaxArea 公式)。
pub(crate) fn largest_inscribed_rect(w: f64, h: f64, angle: f64) -> (f64, f64) {
    if w <= 0.0 || h <= 0.0 {
        return (0.0, 0.0);
    }
    let width_is_longer = w >= h;
    let (side_long, side_short) = if width_is_longer { (w, h) } else { (h, w) };

    let sin_a = angle.sin().abs();
    let cos_a = angle.cos().abs();

    if side_short <= 2.0 * sin_a * cos_a * side_long || (sin_a - cos_a).abs() < 1e-10 {
        // 半分に折れた三角形が支配的なケース
        let x = 0.5 * side_short;
        if width_is_longer {
            (
                x / sin_a.max(f64::MIN_POSITIVE),
                x / cos_a.max(f64::MIN_POSITIVE),
            )
        } else {
            (
                x / cos_a.max(f64::MIN_POSITIVE),
                x / sin_a.max(f64::MIN_POSITIVE),
            )
        }
    } else {
        let cos_2a = cos_a * cos_a - sin_a * sin_a;
        (
            (w * cos_a - h * sin_a) / cos_2a,
            (h * cos_a - w * sin_a) / cos_2a,
        )
    }
}

/// outer の中に inner を anchor に従って配置したときの左上オフセット。
fn anchor_offset(outer: u32, inner: u32, anchor: Anchor, horizontal: bool) -> u32 {
    let slack = outer.saturating_sub(inner);
    let start = matches!(
        (anchor, horizontal),
        (Anchor::Left, true)
            | (Anchor::TopLeft, true)
            | (Anchor::BottomLeft, true)
            | (Anchor::Top, false)
            | (Anchor::TopLeft, false)
            | (Anchor::TopRight, false)
    );
    let end = matches!(
        (anchor, horizontal),
        (Anchor::Right, true)
            | (Anchor::TopRight, true)
            | (Anchor::BottomRight, true)
            | (Anchor::Bottom, false)
            | (Anchor::BottomLeft, false)
            | (Anchor::BottomRight, false)
    );
    if start {
        0
    } else if end {
        slack
    } else {
        slack / 2
    }
}

/// アスペクト比合わせを1回だけ適用したときの寸法(丸め込み込み)。
///
/// どちらの辺を固定するかは `current` と `target` の大小比較で決まるため、
/// 丸め後の実効比率次第では次の適用で分岐が反転しうる(→ `fit_aspect_dims` で
/// 不動点まで反復する)。
fn fit_aspect_step(w: u32, h: u32, target: f64, mode: CropMode) -> (u32, u32) {
    let dim = |v: f64| -> u32 { v.round().clamp(1.0, u32::MAX as f64) as u32 };
    let current = w as f64 / h as f64;
    match mode {
        // 横に長い → 幅を詰める / 縦に長い → 高さを詰める
        CropMode::Crop => {
            if current > target {
                (dim(h as f64 * target), h)
            } else {
                (w, dim(w as f64 / target))
            }
        }
        // 横に長い → 上下に余白 / 縦に長い → 左右に余白
        CropMode::Pad => {
            if current > target {
                (w, dim(w as f64 / target))
            } else {
                (dim(h as f64 * target), h)
            }
        }
    }
}

/// アスペクト比合わせ後の寸法を、**不動点に収束するまで**反復して求める。
///
/// 1回だけ計算すると、丸めによって実効比率が target をわずかに跨いだ場合に
/// 次回適用で「逆の辺」が動いてしまい冪等性が壊れる(例: 8x8 に "1:6" →
/// (1,8) → (1,6))。ここで不動点まで詰めておくことで、1回の適用で安定寸法
/// ((8,8) + "1:6" → (1,6))に到達し、2回目以降は何も変えない。
///
/// 収束性: Crop では寸法が単調非増加、Pad では単調非減少で、いずれも1回の
/// 変化で少なくとも一方の辺が動くため必ず停止する(実測では変化は高々2回)。
pub(crate) fn fit_aspect_dims(w: u32, h: u32, ratio: (u32, u32), mode: CropMode) -> (u32, u32) {
    /// 反復回数の安全上限(総当たり検証では高々2回で収束する)。
    const MAX_ITER: usize = 8;

    let target = ratio.0 as f64 / ratio.1 as f64;
    let (mut cw, mut ch) = (w, h);
    for i in 0..MAX_ITER {
        let (nw, nh) = fit_aspect_step(cw, ch, target, mode);
        // Crop は元画像より大きくならず、Pad は元画像より小さくならない。
        let (nw, nh) = match mode {
            CropMode::Crop => (nw.min(w), nh.min(h)),
            CropMode::Pad => (nw.max(w), nh.max(h)),
        };
        if (nw, nh) == (cw, ch) {
            return (cw, ch);
        }
        (cw, ch) = (nw, nh);
        debug_assert!(
            i + 1 < MAX_ITER,
            "fit_aspect_dims did not converge within {MAX_ITER} iterations \
             ({w}x{h}, ratio {}:{}, mode {mode:?})",
            ratio.0,
            ratio.1
        );
    }
    (cw, ch)
}

/// アスペクト比合わせ。mode=Crop なら切り落とし、mode=Pad なら余白を足す。
///
/// 第2要素は「入力座標 → 出力座標」のアフィン変換。
/// crop は左上オフセットぶんの負の平行移動、pad は正の平行移動になる。
pub(crate) fn fit_aspect(
    img: &RgbaImage,
    ratio: (u32, u32),
    anchor: Anchor,
    mode: CropMode,
    pad: [u8; 4],
) -> (RgbaImage, Affine) {
    let (w, h) = img.dimensions();
    let (tw, th) = fit_aspect_dims(w, h, ratio, mode);

    match mode {
        CropMode::Crop => {
            let x = anchor_offset(w, tw, anchor, true);
            let y = anchor_offset(h, th, anchor, false);
            (
                image::imageops::crop_imm(img, x, y, tw, th).to_image(),
                Affine::translate(-(x as f64), -(y as f64)),
            )
        }
        CropMode::Pad => {
            let x = anchor_offset(tw, w, anchor, true);
            let y = anchor_offset(th, h, anchor, false);
            let mut canvas = RgbaImage::from_pixel(tw, th, Rgba(pad));
            image::imageops::replace(&mut canvas, img, x as i64, y as i64);
            (canvas, Affine::translate(x as f64, y as f64))
        }
    }
}

/// 明示矩形クロップ。範囲外はエラー(呼び出し側が op index を付けて包む)。
pub(crate) fn crop_rect(img: &RgbaImage, rect: Rect) -> OpResult<RgbaImage> {
    let (w, h) = img.dimensions();
    let right = rect.x.checked_add(rect.width);
    let bottom = rect.y.checked_add(rect.height);
    match (right, bottom) {
        (Some(r), Some(b)) if r <= w && b <= h => {
            Ok(image::imageops::crop_imm(img, rect.x, rect.y, rect.width, rect.height).to_image())
        }
        _ => Err(format!(
            "crop rect {}x{}+{}+{} is out of bounds for image {}x{}",
            rect.width, rect.height, rect.x, rect.y, w, h
        )),
    }
}

/// リサイズ後の目標寸法(スケール後の寸法, 最終クロップ寸法)を求める。
///
/// - `Cover`: 指定ボックスを覆うまで拡縮 → 中央クロップ(両辺指定時のみクロップ)
/// - `Contain`: 指定ボックスに収まるまで拡縮(クロップなし)
/// - `Fill`: 比率無視で指定寸法へ
/// - `without_enlargement`: 拡大方向のスケールを 1.0 に制限
pub(crate) fn resize_targets(
    iw: u32,
    ih: u32,
    width: Option<u32>,
    height: Option<u32>,
    fit: Fit,
    without_enlargement: bool,
) -> ((u32, u32), (u32, u32)) {
    let clamp_dim = |v: f64| -> u32 { v.round().max(1.0).min(u32::MAX as f64) as u32 };

    match fit {
        Fit::Fill => {
            let mut tw = width.unwrap_or(iw);
            let mut th = height.unwrap_or(ih);
            if without_enlargement {
                tw = tw.min(iw);
                th = th.min(ih);
            }
            ((tw, th), (tw, th))
        }
        Fit::Contain | Fit::Cover => {
            let sx = width.map(|w| w as f64 / iw as f64);
            let sy = height.map(|h| h as f64 / ih as f64);
            let mut scale = match (sx, sy, fit) {
                (Some(a), Some(b), Fit::Cover) => a.max(b),
                (Some(a), Some(b), _) => a.min(b),
                (Some(a), None, _) => a,
                (None, Some(b), _) => b,
                (None, None, _) => 1.0,
            };
            if without_enlargement && scale > 1.0 {
                scale = 1.0;
            }
            let sw = clamp_dim(iw as f64 * scale);
            let sh = clamp_dim(ih as f64 * scale);
            let crop = if fit == Fit::Cover {
                match (width, height) {
                    // 両辺指定の cover のみ、はみ出しを中央クロップする。
                    (Some(w), Some(h)) => (w.min(sw), h.min(sh)),
                    _ => (sw, sh),
                }
            } else {
                (sw, sh)
            };
            ((sw, sh), crop)
        }
    }
}

/// Lanczos3 によるリサイズ。`has_alpha=false` のときはアルファ処理を省く。
pub(crate) fn resize_lanczos3(
    img: &RgbaImage,
    dst_w: u32,
    dst_h: u32,
    has_alpha: bool,
) -> OpResult<RgbaImage> {
    let (iw, ih) = img.dimensions();
    if (iw, ih) == (dst_w, dst_h) {
        return Ok(img.clone());
    }
    let src = ImageRef::new(iw, ih, img.as_raw(), PixelType::U8x4).map_err(|e| e.to_string())?;
    let mut dst = FirImage::new(dst_w, dst_h, PixelType::U8x4);
    let options = ResizeOptions::new()
        .resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3))
        .use_alpha(has_alpha);
    Resizer::new()
        .resize(&src, &mut dst, &options)
        .map_err(|e| e.to_string())?;
    RgbaImage::from_raw(dst_w, dst_h, dst.into_vec())
        .ok_or_else(|| "resized buffer has unexpected length".to_string())
}

/// 色調整。すべて RGBA8 上の決定論的な固定手順で行う(適用順は下記の通り)。
///
/// 1. brightness `b` : `v' = v + b*255`(線形オフセット)
/// 2. contrast   `c` : `v' = (v-128)*(1+c) + 128`(128 を軸としたスケール)
/// 3. saturation `s` : `v' = luma + (v-luma)*(1+s)`(Rec.601 luma との線形補間)
/// 4. sharpness  `k` : 3x3 ガウシアン `[1 2 1; 2 4 2; 1 2 1]/16` によるアンシャープマスク
///    `v' = v + k*(v - blur)`
///
/// アルファチャンネルは変更しない。美的な最適化より決定性と単純さを優先している。
pub(crate) fn adjust(
    img: &RgbaImage,
    brightness: f64,
    contrast: f64,
    saturation: f64,
    sharpness: f64,
) -> RgbaImage {
    let mut out = img.clone();

    if brightness != 0.0 || contrast != 0.0 || saturation != 0.0 {
        let b = (brightness * 255.0) as f32;
        let c = (1.0 + contrast) as f32;
        let s = (1.0 + saturation) as f32;
        for px in out.pixels_mut() {
            let mut rgb = [px.0[0] as f32, px.0[1] as f32, px.0[2] as f32];
            for v in &mut rgb {
                *v += b;
                *v = (*v - 128.0) * c + 128.0;
            }
            if saturation != 0.0 {
                let luma = 0.299 * rgb[0] + 0.587 * rgb[1] + 0.114 * rgb[2];
                for v in &mut rgb {
                    *v = luma + (*v - luma) * s;
                }
            }
            for (i, v) in rgb.iter().enumerate() {
                px.0[i] = v.clamp(0.0, 255.0).round() as u8;
            }
        }
    }

    if sharpness > 0.0 {
        out = unsharp_mask(&out, sharpness as f32);
    }
    out
}

/// 3x3 ガウシアンぼかしとの差分を `amount` 倍して戻すアンシャープマスク。
fn unsharp_mask(img: &RgbaImage, amount: f32) -> RgbaImage {
    const KERNEL: [[f32; 3]; 3] = [[1.0, 2.0, 1.0], [2.0, 4.0, 2.0], [1.0, 2.0, 1.0]];
    let (w, h) = img.dimensions();
    let mut out = img.clone();
    if w == 0 || h == 0 {
        return out;
    }
    for y in 0..h {
        for x in 0..w {
            let mut acc = [0.0f32; 3];
            for (ky, row) in KERNEL.iter().enumerate() {
                // 端はクランプ(最近傍複製)で外挿する。
                let sy = (y as i64 + ky as i64 - 1).clamp(0, h as i64 - 1) as u32;
                for (kx, weight) in row.iter().enumerate() {
                    let sx = (x as i64 + kx as i64 - 1).clamp(0, w as i64 - 1) as u32;
                    let p = img.get_pixel(sx, sy);
                    for (i, a) in acc.iter_mut().enumerate() {
                        *a += p.0[i] as f32 * weight;
                    }
                }
            }
            let dst = out.get_pixel_mut(x, y);
            let src = img.get_pixel(x, y);
            for (i, a) in acc.iter().enumerate() {
                let blur = a / 16.0;
                let v = src.0[i] as f32 + amount * (src.0[i] as f32 - blur);
                dst.0[i] = v.clamp(0.0, 255.0).round() as u8;
            }
        }
    }
    out
}
