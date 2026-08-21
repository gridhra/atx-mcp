//! 画素レベルの決定論的オペレーション(向き正規化・回転・クロップ・リサイズ・色調整)。
//!
//! v2 の中間表現は `crate::linear::LinearImage`(f32 RGBA、ストレートアルファ)。
//! 幾何系(回転・リサイズ・クロップ・パッド)は**線形光**で動き、
//! `adjust` だけは v1 と同じ知覚的な効き方を保つため **sRGB 符号値空間**で動く
//! (作業空間の一覧は `crate::ops` のモジュールドキュメントを参照)。
//!
//! 決定論規約(`tests/f32_spike.rs` の契約): `mul_add` を使わない / 総和は固定順序 /
//! カーネル係数は f64 で計算してから 1e-6 グリッドへ量子化する。
//! 行分割の並列化は画素間の実行順序を変えるだけなので出力に影響しない。

use image::{ImageBuffer, Rgba};
use imageproc::geometric_transformations::{
    rotate_about_center, rotate_about_center_no_crop, Interpolation,
};

use crate::linear::{quantize_1e6, LinearImage};
use crate::parallel;
use crate::recipe::{Anchor, CropMode, Fit, Rect, RotateCrop};
use crate::transform::Affine;

/// op ヘルパの失敗。呼び出し側(engine)が op index / op 名を付けて
/// `AtxError::Operation` に包み直す。
type OpResult<T> = std::result::Result<T, String>;

/// `imageproc` の warp に渡すための f32 RGBA バッファ。
pub(crate) type F32Image = ImageBuffer<Rgba<f32>, Vec<f32>>;

/// `LinearImage` → `image` の f32 バッファ(warp 用の一時表現)。
pub(crate) fn to_f32_image(img: &LinearImage) -> F32Image {
    let mut raw = Vec::with_capacity(img.data.len() * 4);
    for px in &img.data {
        raw.extend_from_slice(px);
    }
    ImageBuffer::from_raw(img.width, img.height, raw)
        .expect("buffer sized w*h*4 matches Rgba<f32> layout")
}

/// `image` の f32 バッファ → `LinearImage`。
pub(crate) fn from_f32_image(img: &F32Image) -> LinearImage {
    let (width, height) = img.dimensions();
    LinearImage {
        width,
        height,
        data: img.pixels().map(|p| p.0).collect(),
    }
}

// --------------------------------------------------------------- orientation

/// EXIF Orientation(1-8)を画素へ焼き込む。1 / 未知の値は無変換。
///
/// v1 は `image::imageops` に委譲していたが、v2 の中間表現が f32 RGBA になったので
/// 添字の並べ替えとして直接実装する(補間は挟まらないので厳密・可逆)。
pub(crate) fn apply_orientation(img: LinearImage, orientation: u16) -> LinearImage {
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

fn flip_horizontal(img: &LinearImage) -> LinearImage {
    let (w, h) = img.dimensions();
    let mut out = LinearImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            out.set(x, y, img.get(w - 1 - x, y));
        }
    }
    out
}

fn flip_vertical(img: &LinearImage) -> LinearImage {
    let (w, h) = img.dimensions();
    let mut out = LinearImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            out.set(x, y, img.get(x, h - 1 - y));
        }
    }
    out
}

/// 時計回り 90 度。出力寸法は入れ替わる。
pub(crate) fn rotate90(img: &LinearImage) -> LinearImage {
    let (w, h) = img.dimensions();
    let mut out = LinearImage::new(h, w);
    for y in 0..h {
        for x in 0..w {
            out.set(h - 1 - y, x, img.get(x, y));
        }
    }
    out
}

pub(crate) fn rotate180(img: &LinearImage) -> LinearImage {
    let (w, h) = img.dimensions();
    let mut out = LinearImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            out.set(w - 1 - x, h - 1 - y, img.get(x, y));
        }
    }
    out
}

pub(crate) fn rotate270(img: &LinearImage) -> LinearImage {
    let (w, h) = img.dimensions();
    let mut out = LinearImage::new(h, w);
    for y in 0..h {
        for x in 0..w {
            out.set(y, w - 1 - x, img.get(x, y));
        }
    }
    out
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

// -------------------------------------------------------------------- rotate

/// 任意角度回転。正の角度 = 時計回り。
///
/// - `crop = Full`: 回転後の外接矩形をキャンバスとし、余白は `pad` 色で塗る
/// - `crop = LargestInscribedRect`: 回転後の最大内接矩形(軸並行)で中央クロップ
///
/// 戻り値の第2要素は警告(内接矩形クロップで失われた画素の割合)、
/// 第3要素は「入力座標 → 出力座標」のアフィン変換(`coordinate_space: source` 用)。
///
/// # v2 の変更点
///
/// 補間(bicubic)は **線形光の f32 上で、アルファをプリマルチプライしてから**
/// 実行する。v1 は sRGB 符号値の u8 上で非プリマルチプライのまま補間していたため、
/// (a) 明暗の境界が知覚的に暗く沈み、(b) 半透明の縁で背景色が滲んでいた。
/// アルファが全画素 1.0 の画像ではプリマルチプライは 1.0 倍 = 厳密な恒等なので、
/// 不透明画像に対しては専用の高速パスを持たなくても結果はビット同一になる。
///
/// `pad` は**線形光の f32 画素**(呼び出し側で `linear::pad_to_linear` 済み)。
///
/// # 変換の導出(v1 から不変)
///
/// - 90 の倍数: 添字の並べ替えによる厳密回転。中心 `(w/2, h/2)` → `(ow/2, oh/2)`。
/// - 任意角: `imageproc` の `warp` 系は **index 座標** で中心を `(w/2, h/2)` と置くため、
///   連続座標へ換算した中心 `(w/2 + 0.5, h/2 + 0.5)` を使う。
/// - `LargestInscribedRect`: 上の回転に続けて内接矩形の左上 `(x, y)` ぶんの
///   **負の平行移動**を合成する。
/// - `Full`: 出力キャンバスが広がり、`imageproc` は出力中心を `(ow/2, oh/2)` に置く。
pub(crate) fn rotate(
    img: &LinearImage,
    angle_degrees: f64,
    crop: RotateCrop,
    pad: [f32; 4],
) -> (LinearImage, Option<String>, Affine) {
    let (w, h) = img.dimensions();
    // 90 の倍数は補間を挟まない厳密な回転で処理する(劣化・端の丸め誤差を避ける)。
    // 内接矩形も全体キャンバスも一致するため crop の区別は不要。
    if angle_degrees % 90.0 == 0.0 {
        let quarter = (angle_degrees / 90.0).rem_euclid(4.0) as u32;
        let out = match quarter {
            1 => rotate90(img),
            2 => rotate180(img),
            3 => rotate270(img),
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
    // 補間はプリマルチプライ空間で行う。境界色も同じ空間へ持ち込む。
    let premul = img.premultiplied();
    let pad_premul = Rgba([pad[0] * pad[3], pad[1] * pad[3], pad[2] * pad[3], pad[3]]);
    let border = imageproc::geometric_transformations::Border::Constant(pad_premul);
    let src = to_f32_image(&premul);

    match crop {
        RotateCrop::Full => {
            let warped = rotate_about_center_no_crop(&src, theta, Interpolation::Bicubic, border);
            let mut out = from_f32_image(&warped);
            out.unpremultiply();
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
            let warped = rotate_about_center(&src, theta, Interpolation::Bicubic, border);
            let mut rotated = from_f32_image(&warped);
            rotated.unpremultiply();
            let (rw, rh) = largest_inscribed_rect(w as f64, h as f64, angle_degrees.to_radians());
            let rw = (rw.floor() as u32).clamp(1, w);
            let rh = (rh.floor() as u32).clamp(1, h);
            let x = (w - rw) / 2;
            let y = (h - rh) / 2;
            let cropped = crop_view(&rotated, x, y, rw, rh);
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

// ---------------------------------------------------------------- crop / pad

/// 矩形の切り出し(範囲は呼び出し側で検証済みであること)。
pub(crate) fn crop_view(img: &LinearImage, x: u32, y: u32, w: u32, h: u32) -> LinearImage {
    let mut out = LinearImage::new(w, h);
    for oy in 0..h {
        for ox in 0..w {
            out.set(ox, oy, img.get(x + ox, y + oy));
        }
    }
    out
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
/// `pad` は現在の作業空間の f32 画素。
pub(crate) fn fit_aspect(
    img: &LinearImage,
    ratio: (u32, u32),
    anchor: Anchor,
    mode: CropMode,
    pad: [f32; 4],
) -> (LinearImage, Affine) {
    let (w, h) = img.dimensions();
    let (tw, th) = fit_aspect_dims(w, h, ratio, mode);

    match mode {
        CropMode::Crop => {
            let x = anchor_offset(w, tw, anchor, true);
            let y = anchor_offset(h, th, anchor, false);
            (
                crop_view(img, x, y, tw, th),
                Affine::translate(-(x as f64), -(y as f64)),
            )
        }
        CropMode::Pad => {
            let x = anchor_offset(tw, w, anchor, true);
            let y = anchor_offset(th, h, anchor, false);
            let mut canvas = LinearImage::from_pixel(tw, th, pad);
            for sy in 0..h {
                for sx in 0..w {
                    canvas.set(x + sx, y + sy, img.get(sx, sy));
                }
            }
            (canvas, Affine::translate(x as f64, y as f64))
        }
    }
}

/// 明示矩形クロップ。範囲外はエラー(呼び出し側が op index を付けて包む)。
pub(crate) fn crop_rect(img: &LinearImage, rect: Rect) -> OpResult<LinearImage> {
    let (w, h) = img.dimensions();
    let right = rect.x.checked_add(rect.width);
    let bottom = rect.y.checked_add(rect.height);
    match (right, bottom) {
        (Some(r), Some(b)) if r <= w && b <= h => {
            Ok(crop_view(img, rect.x, rect.y, rect.width, rect.height))
        }
        _ => Err(format!(
            "crop rect {}x{}+{}+{} is out of bounds for image {}x{}",
            rect.width, rect.height, rect.x, rect.y, w, h
        )),
    }
}

// -------------------------------------------------------------------- resize

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

/// Lanczos3 の窓関数(`a = 3`)。f64 で計算する(呼ばれるのは係数表の構築時のみ)。
fn lanczos3(x: f64) -> f64 {
    const A: f64 = 3.0;
    if x.abs() < 1e-12 {
        return 1.0;
    }
    if x.abs() >= A {
        return 0.0;
    }
    let pix = std::f64::consts::PI * x;
    // sinc(x) * sinc(x/a)
    (pix.sin() / pix) * ((pix / A).sin() / (pix / A))
}

/// 1 出力画素あたりの寄与(開始ソース添字と重み列)。
struct Contribution {
    start: i64,
    weights: Vec<f32>,
}

/// 1 軸ぶんの Lanczos3 係数表を作る。
///
/// 決定論規約:
/// 1. 係数は f64 の `sin` で計算する(**構築時のみ**。画素ループでは呼ばない)
/// 2. 各係数を 1e-6 グリッドへ量子化する
/// 3. 量子化後の合計で正規化する(量子化 → 合計 → 除算 の順序を固定)
///
/// 縮小時はカーネルを縮小率ぶん広げる(`filter_scale = src / dst`)。これが
/// ローパスとして働き、エイリアスを抑える。拡大時は `filter_scale = 1`。
fn build_contributions(src_len: u32, dst_len: u32) -> Vec<Contribution> {
    let ratio = src_len as f64 / dst_len as f64;
    let filter_scale = if ratio > 1.0 { ratio } else { 1.0 };
    let support = 3.0 * filter_scale;

    (0..dst_len)
        .map(|o| {
            // pixel-center 整列: 出力画素中心 (o + 0.5) を入力の連続座標へ写す。
            let center = (o as f64 + 0.5) * ratio;
            let start = (center - support - 0.5).ceil() as i64;
            let end = (center + support - 0.5).floor() as i64;
            let mut raw: Vec<f64> = Vec::with_capacity((end - start + 1).max(1) as usize);
            for i in start..=end {
                let t = (i as f64 + 0.5 - center) / filter_scale;
                raw.push(quantize_1e6(lanczos3(t)));
            }
            // 量子化後の合計で正規化する(この順序が決定論の要)。
            let sum: f64 = raw.iter().fold(0.0f64, |acc, w| acc + w);
            let weights: Vec<f32> = if sum != 0.0 {
                raw.iter().map(|w| (w / sum) as f32).collect()
            } else {
                raw.iter().map(|w| *w as f32).collect()
            };
            Contribution { start, weights }
        })
        .collect()
}

/// Lanczos3 による分離可能リサイズ(横 → 縦の 2 パス)。
///
/// **v2 の変更点**: 入力は線形光の f32 で、アルファをプリマルチプライしてから
/// 畳み込む。v1 は `fast_image_resize` に u8 の sRGB 符号値を渡していたため、
/// 「符号値の平均」= 物理的に誤った暗部寄りの平均になっていた
/// (白黒市松を縮小すると sRGB 128 = 線形 0.216 になり、正しい線形 0.5 = sRGB 188 に
/// ならないという古典的なガンマ・ブラー誤差。tests/engine_v2_quality.rs で検証)。
///
/// アルファが全画素 1.0 の画像ではプリマルチプライは厳密な恒等なので、
/// 不透明画像でも専用パスなしで同じ結果になる。
pub(crate) fn resize_lanczos3(img: &LinearImage, dst_w: u32, dst_h: u32) -> OpResult<LinearImage> {
    let (iw, ih) = img.dimensions();
    if iw == 0 || ih == 0 || dst_w == 0 || dst_h == 0 {
        return Err(format!(
            "cannot resize {iw}x{ih} to {dst_w}x{dst_h} (zero dimension)"
        ));
    }
    if (iw, ih) == (dst_w, dst_h) {
        return Ok(img.clone());
    }

    let premul = img.premultiplied();

    // --- パス1: 水平 (iw -> dst_w) ---
    let hx = build_contributions(iw, dst_w);
    let mut horiz = vec![[0f32; 4]; dst_w as usize * ih as usize];
    parallel::fill_rows(&mut horiz, dst_w as usize, ih as usize, |y, row| {
        for (ox, slot) in row.iter_mut().enumerate() {
            let c = &hx[ox];
            let mut acc = [0f32; 4];
            for (k, &weight) in c.weights.iter().enumerate() {
                let sx = (c.start + k as i64).clamp(0, iw as i64 - 1) as u32;
                let px = premul.data[y * iw as usize + sx as usize];
                // 乗算 → 加算 を分離(mul_add 禁止)、走査順そのままの左結合。
                for ch in 0..4 {
                    let term = px[ch] * weight;
                    acc[ch] += term;
                }
            }
            *slot = acc;
        }
    });

    // --- パス2: 垂直 (ih -> dst_h) ---
    let vy = build_contributions(ih, dst_h);
    let mut out_data = vec![[0f32; 4]; dst_w as usize * dst_h as usize];
    parallel::fill_rows(&mut out_data, dst_w as usize, dst_h as usize, |oy, row| {
        let c = &vy[oy];
        for (ox, slot) in row.iter_mut().enumerate() {
            let mut acc = [0f32; 4];
            for (k, &weight) in c.weights.iter().enumerate() {
                let sy = (c.start + k as i64).clamp(0, ih as i64 - 1) as usize;
                let px = horiz[sy * dst_w as usize + ox];
                for ch in 0..4 {
                    let term = px[ch] * weight;
                    acc[ch] += term;
                }
            }
            *slot = acc;
        }
    });

    let mut out = LinearImage {
        width: dst_w,
        height: dst_h,
        data: out_data,
    };
    out.unpremultiply();
    Ok(out)
}

// -------------------------------------------------------------------- adjust

/// contrast の軸(v1 の 128/255 をそのまま f32 の 0..1 へ写したもの)。
const CONTRAST_PIVOT: f32 = 128.0 / 255.0;

/// 色調整。**sRGB 符号値空間**の f32 に対して、v1 と同じ順序・同じ係数で適用する。
///
/// 1. brightness `b` : `v' = v + b`(v1 の `v + b*255` を 0..1 へ写したもの)
/// 2. contrast   `c` : `v' = (v - 128/255)*(1+c) + 128/255`
/// 3. saturation `s` : `v' = luma + (v-luma)*(1+s)`(Rec.601 luma との線形補間)
/// 4. sharpness  `k` : 3x3 ガウシアン `[1 2 1; 2 4 2; 1 2 1]/16` によるアンシャープマスク
///    `v' = v + k*(v - blur)`
///
/// アルファチャンネルは変更しない。
///
/// # v1 との差
///
/// 数式・係数・適用順は同一だが、**中間結果が u8 に丸められない**。
/// v1 では 1〜3 の合成後に 256 段へ量子化されていたため、adjust を重ねると
/// ポスタリゼーションが蓄積した。v2 では f32 のまま次の op へ渡る。
/// なお `sharpness` の 3x3 ぼかしは v1 と同じく **RGB のみ・非プリマルチプライ**で
/// 掛ける(アルファ形状を触らない意図的な簡略化。半透明の縁を厳密に扱いたい要求は
/// `unsharp_mask` op 側で扱う)。
pub(crate) fn adjust(
    img: &LinearImage,
    brightness: f64,
    contrast: f64,
    saturation: f64,
    sharpness: f64,
) -> LinearImage {
    let mut out = img.clone();

    if brightness != 0.0 || contrast != 0.0 || saturation != 0.0 {
        let b = brightness as f32;
        let c = (1.0 + contrast) as f32;
        let s = (1.0 + saturation) as f32;
        let apply_saturation = saturation != 0.0;
        parallel::for_each_chunk(&mut out.data, |chunk| {
            for px in chunk.iter_mut() {
                let mut rgb = [px[0], px[1], px[2]];
                for v in &mut rgb {
                    *v += b;
                    *v = (*v - CONTRAST_PIVOT) * c + CONTRAST_PIVOT;
                }
                if apply_saturation {
                    let luma = 0.299 * rgb[0] + 0.587 * rgb[1] + 0.114 * rgb[2];
                    for v in &mut rgb {
                        *v = luma + (*v - luma) * s;
                    }
                }
                for (i, v) in rgb.iter().enumerate() {
                    px[i] = v.clamp(0.0, 1.0);
                }
            }
        });
    }

    if sharpness > 0.0 {
        out = unsharp_3x3(&out, sharpness as f32);
    }
    out
}

/// 3x3 ガウシアンぼかしとの差分を `amount` 倍して戻すアンシャープマスク(RGB のみ)。
fn unsharp_3x3(img: &LinearImage, amount: f32) -> LinearImage {
    const KERNEL: [[f32; 3]; 3] = [[1.0, 2.0, 1.0], [2.0, 4.0, 2.0], [1.0, 2.0, 1.0]];
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return img.clone();
    }
    let mut out_data = vec![[0f32; 4]; w as usize * h as usize];
    parallel::fill_rows(&mut out_data, w as usize, h as usize, |y, row| {
        for (x, slot) in row.iter_mut().enumerate() {
            let mut acc = [0.0f32; 3];
            for (ky, krow) in KERNEL.iter().enumerate() {
                // 端はクランプ(最近傍複製)で外挿する。
                let sy = y as i64 + ky as i64 - 1;
                for (kx, weight) in krow.iter().enumerate() {
                    let sx = x as i64 + kx as i64 - 1;
                    let p = img.get_clamped(sx, sy);
                    for (i, a) in acc.iter_mut().enumerate() {
                        let term = p[i] * weight;
                        *a += term;
                    }
                }
            }
            let src = img.get(x as u32, y as u32);
            let mut px = src;
            for (i, a) in acc.iter().enumerate() {
                let blur = a / 16.0;
                let diff = src[i] - blur;
                let boost = amount * diff;
                px[i] = (src[i] + boost).clamp(0.0, 1.0);
            }
            *slot = px;
        }
    });
    LinearImage {
        width: w,
        height: h,
        data: out_data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lanczos3_is_one_at_zero_and_zero_at_integers() {
        assert_eq!(lanczos3(0.0), 1.0);
        for k in 1..=3 {
            assert!(
                lanczos3(k as f64).abs() < 1e-12,
                "lanczos3({k}) must vanish"
            );
        }
        assert_eq!(lanczos3(4.0), 0.0);
    }

    #[test]
    fn contributions_are_normalized() {
        for (src, dst) in [(256u32, 173u32), (100, 400), (7, 7), (1477, 1200)] {
            for c in build_contributions(src, dst) {
                let sum: f32 = c.weights.iter().fold(0.0f32, |a, w| a + w);
                assert!(
                    (sum - 1.0).abs() < 1e-5,
                    "weights for {src}->{dst} sum to {sum}"
                );
            }
        }
    }

    /// 均一色は縮小しても(重み和が 1 なので)同じ値に留まる。
    #[test]
    fn resize_of_uniform_image_is_uniform() {
        let img = LinearImage::from_pixel(64, 64, [0.25, 0.5, 0.75, 1.0]);
        let out = resize_lanczos3(&img, 17, 9).unwrap();
        for px in &out.data {
            for (a, b) in px.iter().zip([0.25f32, 0.5, 0.75, 1.0]) {
                assert!((a - b).abs() < 1e-5, "{px:?} drifted from the source color");
            }
        }
    }

    #[test]
    fn quarter_rotations_are_exact_permutations() {
        let mut img = LinearImage::new(3, 2);
        for y in 0..2 {
            for x in 0..3 {
                img.set(x, y, [x as f32, y as f32, 0.0, 1.0]);
            }
        }
        let four = rotate90(&rotate90(&rotate90(&rotate90(&img))));
        assert_eq!(four, img);
        assert_eq!(rotate180(&rotate180(&img)), img);
        assert_eq!(rotate270(&rotate90(&img)), img);
    }
}
