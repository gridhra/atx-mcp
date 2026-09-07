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

pub fn validate(index: usize, background: &Option<String>, padding: u32) -> Result<()> {
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

/// 切り出すべき矩形を求める。内容が1画素も無ければ `None`(呼び出し側で恒等 + 警告)。
///
/// `background` が `Some` ならその色、`None` なら四隅の多数決を背景とする。
/// 得られた外接矩形に `padding` を足し、画像内へクランプする。
pub fn content_rect(
    img: &LinearImage,
    tolerance: u8,
    background: Option<[u8; 4]>,
    padding: u32,
) -> Option<Rect> {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    let bg = background.unwrap_or_else(|| corner_background(img));

    // 4 辺から内側へ走査するのと、内容画素の外接矩形を取るのは同値。
    // 1 パスで済む後者で書く(走査順は行優先で固定)。
    let mut min_x = u32::MAX;
    let mut min_y = u32::MAX;
    let mut max_x = 0u32;
    let mut max_y = 0u32;
    let mut found = false;
    for y in 0..h {
        for x in 0..w {
            if is_background(to_u8(img.get(x, y)), bg, tolerance) {
                continue;
            }
            found = true;
            if x < min_x {
                min_x = x;
            }
            if x > max_x {
                max_x = x;
            }
            if y < min_y {
                min_y = y;
            }
            if y > max_y {
                max_y = y;
            }
        }
    }
    if !found {
        return None;
    }

    let x0 = min_x.saturating_sub(padding);
    let y0 = min_y.saturating_sub(padding);
    let x1 = max_x.saturating_add(padding).min(w - 1);
    let y1 = max_y.saturating_add(padding).min(h - 1);
    Some(Rect {
        x: x0,
        y: y0,
        width: x1 - x0 + 1,
        height: y1 - y0 + 1,
    })
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
        assert!(validate(0, &None, PADDING_MAX + 1).is_err());
        assert!(validate(0, &None, PADDING_MAX).is_ok());
    }

    #[test]
    fn validate_rejects_bad_hex() {
        assert!(validate(0, &Some("ffffff".to_string()), 0).is_err());
        assert!(validate(0, &Some("#fff".to_string()), 0).is_ok());
    }

    #[test]
    fn content_rect_finds_the_box() {
        let rect = content_rect(&img_with_box(), 16, None, 0).expect("content");
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
        let rect = content_rect(&img_with_box(), 16, None, 100).expect("content");
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
    fn a_uniform_image_has_no_content() {
        let img = LinearImage::from_pixel(4, 4, [0.5, 0.5, 0.5, 1.0]);
        assert!(content_rect(&img, 16, None, 0).is_none());
    }

    #[test]
    fn transparent_pixels_are_background_regardless_of_rgb() {
        let mut img = LinearImage::from_pixel(4, 4, [1.0, 0.0, 0.0, 0.0]);
        img.set(1, 1, [0.0, 0.0, 1.0, 1.0]);
        let rect = content_rect(&img, 16, None, 0).expect("content");
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
