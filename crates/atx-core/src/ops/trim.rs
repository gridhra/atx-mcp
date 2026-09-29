//! trim(余白の自動切り落とし。ImageMagick `-trim` 相当。DESIGN.md §9.12)。
//!
//! 背景色に近い縁を上下左右から走査して落とす**幾何 op**。マスクは取らない。
//!
//! # なぜ u8 格子で判定するのか
//!
//! `tolerance` は「RGBA u8 の Chebyshev 距離」と定義されている(エージェントが
//! 書きやすい単位)。パイプラインの中間表現は f32 なので、そのまま比較すると
//! 「16 / 255 との比較」が丸め位置によって 1 段ぶれる。判定の前に
//! **sRGB 符号値を u8 へ丸めてから**比較することで、閾値の意味が入力バイト列の
//! 表現に依らず確定する(エンジン側が `Space::Srgb` へ移してから呼ぶ)。
//!
//! 判定に浮動小数の比較が一切出てこないので、決定論はここでは自明に保たれる。

use crate::linear::{unit_to_u8, LinearImage};
use crate::recipe::Rect;
use crate::{AtxError::InvalidRecipe, Result};

/// `padding` の上限。
const PADDING_MAX: u32 = 4096;

pub fn validate(
    index: usize,
    background: &Option<String>,
    padding: u32,
    min_content_px: u32,
) -> Result<()> {
    if min_content_px == 0 {
        return Err(InvalidRecipe(format!(
            "operations[{index}] (trim): min_content_px must be at least 1, got 0"
        )));
    }
    if padding > PADDING_MAX {
        return Err(InvalidRecipe(format!(
            "operations[{index}] (trim): padding must be within 0..={PADDING_MAX}, got {padding}"
        )));
    }
    if let Some(hex) = background {
        if crate::recipe::parse_hex_color(hex).is_none() {
            return Err(InvalidRecipe(format!(
                "operations[{index}] (trim): background must be a CSS hex color \
                 (#rgb / #rrggbb / #rrggbbaa), got {hex:?}"
            )));
        }
    }
    Ok(())
}

/// sRGB f32 画素を u8 RGBA へ丸める(判定用の格子)。
#[inline]
fn to_u8(px: [f32; 4]) -> [u8; 4] {
    [
        unit_to_u8(px[0]),
        unit_to_u8(px[1]),
        unit_to_u8(px[2]),
        unit_to_u8(px[3]),
    ]
}

/// 背景と見なすか。RGBA の Chebyshev 距離が `tolerance` 以下なら背景。
///
/// 透明部分の RGB はエンコーダ・デコーダ依存のノイズを持ちうるので、
/// **背景も画素もアルファが `tolerance` 以下**なら RGB を問わず背景と見なす
/// (DESIGN.md §9.12)。
#[inline]
fn is_background(px: [u8; 4], bg: [u8; 4], tolerance: u8) -> bool {
    if bg[3] <= tolerance && px[3] <= tolerance {
        return true;
    }
    let mut d = 0u8;
    for c in 0..4 {
        let diff = px[c].abs_diff(bg[c]);
        if diff > d {
            d = diff;
        }
    }
    d <= tolerance
}

/// 四隅の画素の多数決で背景色を決める(同数なら左上優先)。
///
/// 走査順を tl → tr → bl → br に固定し、**同数のときは先に見たものを採る**
/// (`>` で更新する)ので、`HashMap` の反復順のような不定要素は入らない。
fn corner_background(img: &LinearImage) -> [u8; 4] {
    let (w, h) = img.dimensions();
    let corners = [
        to_u8(img.get(0, 0)),
        to_u8(img.get(w - 1, 0)),
        to_u8(img.get(0, h - 1)),
        to_u8(img.get(w - 1, h - 1)),
    ];
    let mut best = corners[0];
    let mut best_count = 0usize;
    for candidate in corners.iter() {
        let count = corners.iter().filter(|c| *c == candidate).count();
        if count > best_count {
            best_count = count;
            best = *candidate;
        }
    }
    best
}

/// 内容の外接矩形(padding 前)。内容が無ければ `None`(呼び出し側で恒等 + 警告)。
///
/// `background` が `Some` ならその色、`None` なら四隅の多数決を背景とする。
///
/// # `min_content_px` と不動点
///
/// 行(列)は、現在の列(行)範囲の中に背景外画素が `min_content_px` 個以上あるとき
/// 「内容を含む」と数える。行の範囲を決めてから、その行範囲の中で列の範囲を決め、
/// 範囲が変わらなくなるまで繰り返す(範囲は縮むだけなので必ず止まる)。
/// こうして得た矩形は「自分の縁の行・列が自分の範囲の中で条件を満たす」ので、
/// 切った結果にもう一度同じ trim を掛けても動かない(padding = 0 で冪等)。
/// `min_content_px == 1` では 1 回目の反復で全内容画素の外接矩形が得られ、
/// 2 回目で不動点になるため、従来の結果とバイト単位で一致する。
///
/// 判定は u8 格子上の比較と整数の数え上げだけで、浮動小数は出てこない。
pub fn content_bounds(
    img: &LinearImage,
    tolerance: u8,
    background: Option<[u8; 4]>,
    min_content_px: u32,
) -> Option<Rect> {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    let bg = background.unwrap_or_else(|| corner_background(img));
    let min_content = min_content_px.max(1) as usize;

    // 背景外か(行優先で 1 度だけ判定し、以降は数え上げだけ行う)。
    let ww = w as usize;
    let mut content = vec![false; ww * h as usize];
    for y in 0..h {
        for x in 0..w {
            content[y as usize * ww + x as usize] =
                !is_background(to_u8(img.get(x, y)), bg, tolerance);
        }
    }
    let row_has_content = |y: u32, x0: u32, x1: u32| -> bool {
        let row = &content[y as usize * ww..(y as usize + 1) * ww];
        row[x0 as usize..=x1 as usize]
            .iter()
            .filter(|c| **c)
            .count()
            >= min_content
    };
    let col_has_content = |x: u32, y0: u32, y1: u32| -> bool {
        (y0..=y1)
            .filter(|&y| content[y as usize * ww + x as usize])
            .count()
            >= min_content
    };

    let (mut x0, mut y0, mut x1, mut y1) = (0u32, 0u32, w - 1, h - 1);
    loop {
        // 行の範囲(現在の列範囲の中で数える)。
        let ny0 = (y0..=y1).find(|&y| row_has_content(y, x0, x1))?;
        let ny1 = (ny0..=y1).rev().find(|&y| row_has_content(y, x0, x1))?;
        // 列の範囲(新しい行範囲の中で数える)。
        let nx0 = (x0..=x1).find(|&x| col_has_content(x, ny0, ny1))?;
        let nx1 = (nx0..=x1).rev().find(|&x| col_has_content(x, ny0, ny1))?;
        let unchanged = (nx0, ny0, nx1, ny1) == (x0, y0, x1, y1);
        (x0, y0, x1, y1) = (nx0, ny0, nx1, ny1);
        if unchanged {
            break;
        }
    }
    Some(Rect {
        x: x0,
        y: y0,
        width: x1 - x0 + 1,
        height: y1 - y0 + 1,
    })
}

/// 外接矩形に `padding` を足し、`w` x `h` の画像内へクランプする。
pub fn pad_rect(rect: Rect, padding: u32, w: u32, h: u32) -> Rect {
    let x0 = rect.x.saturating_sub(padding);
    let y0 = rect.y.saturating_sub(padding);
    let x1 = (rect.x + rect.width - 1).saturating_add(padding).min(w - 1);
    let y1 = (rect.y + rect.height - 1)
        .saturating_add(padding)
        .min(h - 1);
    Rect {
        x: x0,
        y: y0,
        width: x1 - x0 + 1,
        height: y1 - y0 + 1,
    }
}

/// 切り出すべき矩形(padding 込み)。`content_bounds` + `pad_rect`。
/// エンジンは無変化の判定のために 2 段階で呼ぶので、これは単体テスト用。
#[cfg(test)]
pub fn content_rect(
    img: &LinearImage,
    tolerance: u8,
    background: Option<[u8; 4]>,
    padding: u32,
    min_content_px: u32,
) -> Option<Rect> {
    let (w, h) = img.dimensions();
    content_bounds(img, tolerance, background, min_content_px).map(|r| pad_rect(r, padding, w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img_with_box() -> LinearImage {
        // 8x8 の白背景に、(2,3)-(5,4) の黒い矩形。
        let mut img = LinearImage::from_pixel(8, 8, [1.0, 1.0, 1.0, 1.0]);
        for y in 3..=4 {
            for x in 2..=5 {
                img.set(x, y, [0.0, 0.0, 0.0, 1.0]);
            }
        }
        img
    }

    #[test]
    fn validate_rejects_padding_above_max() {
        assert!(validate(0, &None, PADDING_MAX + 1, 1).is_err());
        assert!(validate(0, &None, PADDING_MAX, 1).is_ok());
    }

    #[test]
    fn validate_rejects_bad_hex() {
        assert!(validate(0, &Some("ffffff".to_string()), 0, 1).is_err());
        assert!(validate(0, &Some("#fff".to_string()), 0, 1).is_ok());
    }

    #[test]
    fn content_rect_finds_the_box() {
        let rect = content_rect(&img_with_box(), 16, None, 0, 1).expect("content");
        assert_eq!(
            rect,
            Rect {
                x: 2,
                y: 3,
                width: 4,
                height: 2
            }
        );
    }

    #[test]
    fn padding_is_clamped_to_the_image() {
        let rect = content_rect(&img_with_box(), 16, None, 100, 1).expect("content");
        assert_eq!(
            rect,
            Rect {
                x: 0,
                y: 0,
                width: 8,
                height: 8
            }
        );
    }

    #[test]
    fn validate_rejects_min_content_px_zero() {
        assert!(validate(0, &None, 0, 0).is_err());
        assert!(validate(0, &None, 0, 1).is_ok());
    }

    /// `min_content_px` の不動点: 縁の行に 1 画素だけ内容があっても、その行は
    /// 「内容を含む」と数えられない(2 未満)ので矩形は本体だけになる。
    /// また、行を捨てた後に列を数え直すので、捨てた行の画素は列の数にも入らない。
    #[test]
    fn min_content_px_drops_rows_and_columns_with_too_few_content_pixels() {
        let mut img = img_with_box();
        // (7, 0): 右上隅近くに 1 画素のゴミ。既定では外接矩形を y=0 / x=7 まで広げる。
        img.set(7, 0, [0.0, 0.0, 0.0, 1.0]);
        let with_default = content_rect(&img, 16, None, 0, 1).expect("content");
        assert_eq!(
            with_default,
            Rect {
                x: 2,
                y: 0,
                width: 6,
                height: 5
            }
        );
        let robust = content_rect(&img, 16, None, 0, 2).expect("content");
        assert_eq!(
            robust,
            Rect {
                x: 2,
                y: 3,
                width: 4,
                height: 2
            }
        );
    }

    #[test]
    fn a_uniform_image_has_no_content() {
        let img = LinearImage::from_pixel(4, 4, [0.5, 0.5, 0.5, 1.0]);
        assert!(content_rect(&img, 16, None, 0, 1).is_none());
    }

    #[test]
    fn transparent_pixels_are_background_regardless_of_rgb() {
        let mut img = LinearImage::from_pixel(4, 4, [1.0, 0.0, 0.0, 0.0]);
        img.set(1, 1, [0.0, 0.0, 1.0, 1.0]);
        let rect = content_rect(&img, 16, None, 0, 1).expect("content");
        assert_eq!(
            rect,
            Rect {
                x: 1,
                y: 1,
                width: 1,
                height: 1
            }
        );
    }
}
