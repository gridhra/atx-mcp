//! 局所適用マスク(v0.5)。マスク画像 revision → 0..1 の重み平面 → op 結果のブレンド。
//!
//! # 設計(DESIGN.md §9.6)
//!
//! - **ブレンドはエンジンの op ループ 1 箇所だけで汎用に行う**。個々の op は
//!   マスクの存在を知らない(op を足してもマスク対応の追加実装が要らない)。
//!   `out = before + (after - before) * w` を **RGBA 4 チャンネルすべて**に、
//!   **その op の作業空間のまま**適用する。
//! - **重みはマスク画像の sRGB 符号値上の BT.709 輝度**。マスクは「光」ではなく
//!   **被覆率(どれだけ適用するか)**なので、線形光へ戻してから輝度を取ると
//!   中間グレー 128 が 0.216 の弱い適用になってしまう。符号値のまま取れば
//!   50% グレー ≒ 0.5 適用となり、作り手の直感と一致する。
//! - マスクの寸法が現在の画像と違えば**双線形補間**で合わせる(係数は f64 →
//!   1e-6 量子化 → f32。`ops/mod.rs` の決定論規約)。
//! - 適用順は **輝度 → リサイズ → invert → feather(ガウス)→ 0..1 クランプ**。
//! - マスクのアルファチャンネルは無視する(被覆率は RGB 輝度だけで表す)。

use std::collections::HashMap;

use crate::engine::AssetResolver;
use crate::linear::{quantize_1e6, LinearImage};
use crate::recipe::MaskRef;
use crate::{AtxError, Result};

/// `feather_px` の上限(現在の画像座標でのガウス σ)。
const MAX_FEATHER_PX: f64 = 200.0;

/// BT.709 輝度係数(sRGB 符号値に対してそのまま掛ける。上のモジュールコメント参照)。
const LUMA_R: f32 = 0.2126;
const LUMA_G: f32 = 0.7152;
const LUMA_B: f32 = 0.0722;

/// マスク参照の静的検証。
pub fn validate(index: usize, mask: &MaskRef) -> Result<()> {
    if mask.revision_id.is_empty() {
        return Err(AtxError::InvalidRecipe(format!(
            "operations[{index}] (mask): revision_id must not be empty"
        )));
    }
    if !mask.revision_id.starts_with("rev_") {
        return Err(AtxError::InvalidRecipe(format!(
            "operations[{index}] (mask): revision_id must start with \"rev_\", got {:?}",
            mask.revision_id
        )));
    }
    if !mask.feather_px.is_finite() || !(0.0..=MAX_FEATHER_PX).contains(&mask.feather_px) {
        return Err(AtxError::InvalidRecipe(format!(
            "operations[{index}] (mask): feather_px must be within 0.0..={MAX_FEATHER_PX}, got {}",
            mask.feather_px
        )));
    }
    Ok(())
}

/// 解決済み重み平面のキャッシュキー。
///
/// 同一の `MaskRef` を複数 op が参照する場合、同じ寸法に対しては 1 回だけ解決する
/// (デコード + リサイズ + フェザは重い)。`feather_px` はビット表現で比較する
/// (validate 済みなので NaN は来ない)。
#[derive(Clone, PartialEq, Eq, Hash)]
struct MaskKey {
    revision_id: String,
    invert: bool,
    feather_bits: u64,
    width: u32,
    height: u32,
}

/// 1 回の `apply_recipe` 呼び出しの中だけで生きるマスク解決キャッシュ。
#[derive(Default)]
pub struct MaskCache {
    entries: HashMap<MaskKey, Vec<f32>>,
}

impl MaskCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// `mask` を `width`×`height` の重み平面(行優先、0..1)へ解決する。
    /// 同じキーが既にあればそれを返す。
    pub fn resolve(
        &mut self,
        mask: &MaskRef,
        width: u32,
        height: u32,
        assets: &dyn AssetResolver,
    ) -> Result<&[f32]> {
        let key = MaskKey {
            revision_id: mask.revision_id.clone(),
            invert: mask.invert,
            feather_bits: mask.feather_px.to_bits(),
            width,
            height,
        };
        if !self.entries.contains_key(&key) {
            let weights = resolve_mask(mask, width, height, assets)?;
            self.entries.insert(key.clone(), weights);
        }
        Ok(&self.entries[&key])
    }
}

/// マスク revision を読み、`width`×`height` の重み平面へ解決する(キャッシュなし)。
pub fn resolve_mask(
    mask: &MaskRef,
    width: u32,
    height: u32,
    assets: &dyn AssetResolver,
) -> Result<Vec<f32>> {
    let id = &mask.revision_id;
    let bytes = assets
        .read_revision(id)
        .map_err(|e| AtxError::InvalidRecipe(format!("mask asset {id} could not be read: {e}")))?;
    let decoded = image::load_from_memory(&bytes).map_err(|e| {
        AtxError::InvalidRecipe(format!(
            "mask asset {id} is not a decodable image (any raster format is accepted): {e}"
        ))
    })?;
    let rgba = decoded.to_rgba8();
    let (mw, mh) = rgba.dimensions();
    if mw == 0 || mh == 0 {
        return Err(AtxError::InvalidRecipe(format!(
            "mask asset {id} has zero dimensions ({mw}x{mh})"
        )));
    }

    // 1) sRGB 符号値上の BT.709 輝度(= 被覆率)。アルファは無視する。
    let mut plane: Vec<f32> = Vec::with_capacity(mw as usize * mh as usize);
    for px in rgba.pixels() {
        let r = px.0[0] as f32 / 255.0;
        let g = px.0[1] as f32 / 255.0;
        let b = px.0[2] as f32 / 255.0;
        // 乗算と加算を分離した固定順序の左結合(FMA 禁止・再結合禁止)。
        let tr = LUMA_R * r;
        let tg = LUMA_G * g;
        let tb = LUMA_B * b;
        let sum = tr + tg;
        plane.push(sum + tb);
    }

    // 2) 現在の画像寸法へ双線形リサイズ(同寸法なら恒等)。
    let mut plane = if (mw, mh) == (width, height) {
        plane
    } else {
        bilinear_resize(&plane, mw, mh, width, height)
    };

    // 3) 反転。
    if mask.invert {
        for w in plane.iter_mut() {
            *w = 1.0 - *w;
        }
    }

    // 4) フェザ(単一チャンネルのガウスぼかし。blur.rs と同じ量子化カーネル)。
    if mask.feather_px > 0.0 {
        plane = feather(&plane, width, height, mask.feather_px);
    }

    // 5) クランプ。
    for w in plane.iter_mut() {
        *w = w.clamp(0.0, 1.0);
    }
    Ok(plane)
}

/// 単一チャンネル平面の双線形リサイズ。
///
/// 写像は画素中心合わせ(`sx = (x + 0.5) * sw / dw - 0.5`)。座標と補間係数は f64 で
/// 求めてから 1e-6 グリッドへ量子化し f32 に落とす(`ops/mod.rs` の決定論規約)。
/// 端はクランプ(複製)。
fn bilinear_resize(src: &[f32], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<f32> {
    let mut out = vec![0f32; dw as usize * dh as usize];
    let scale_x = sw as f64 / dw as f64;
    let scale_y = sh as f64 / dh as f64;
    let at = |x: usize, y: usize| src[y * sw as usize + x];

    for y in 0..dh as usize {
        let sy = (y as f64 + 0.5) * scale_y - 0.5;
        let sy_clamped = sy.clamp(0.0, (sh - 1) as f64);
        let y0 = sy_clamped.floor();
        let iy0 = y0 as usize;
        let iy1 = (iy0 + 1).min(sh as usize - 1);
        let fy = quantize_1e6(sy_clamped - y0) as f32;

        for x in 0..dw as usize {
            let sx = (x as f64 + 0.5) * scale_x - 0.5;
            let sx_clamped = sx.clamp(0.0, (sw - 1) as f64);
            let x0 = sx_clamped.floor();
            let ix0 = x0 as usize;
            let ix1 = (ix0 + 1).min(sw as usize - 1);
            let fx = quantize_1e6(sx_clamped - x0) as f32;

            let a = at(ix0, iy0);
            let b = at(ix1, iy0);
            let c = at(ix0, iy1);
            let d = at(ix1, iy1);
            let top = a + (b - a) * fx;
            let bottom = c + (d - c) * fx;
            out[y * dw as usize + x] = top + (bottom - top) * fy;
        }
    }
    out
}

/// 重み平面に σ = `sigma` のガウスぼかしを掛ける(分離可能、横 → 縦、端はクランプ)。
///
/// カーネルは `ops::blur` と同じ「f64 生成 → 1e-6 量子化 → 量子化後の合計で正規化」で、
/// blur.rs の公開挙動には一切触れずに係数生成だけを共有している。
fn feather(plane: &[f32], width: u32, height: u32, sigma: f64) -> Vec<f32> {
    let (radius, weights) = crate::ops::blur::gaussian_kernel(sigma);
    let (w, h) = (width as usize, height as usize);

    let mut horiz = vec![0f32; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut acc = 0f32;
            for (k, &weight) in weights.iter().enumerate() {
                let dx = k as i64 - radius as i64;
                let sx = (x as i64 + dx).clamp(0, width as i64 - 1) as usize;
                let term = plane[y * w + sx] * weight;
                acc += term;
            }
            horiz[y * w + x] = acc;
        }
    }

    let mut out = vec![0f32; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut acc = 0f32;
            for (k, &weight) in weights.iter().enumerate() {
                let dy = k as i64 - radius as i64;
                let sy = (y as i64 + dy).clamp(0, height as i64 - 1) as usize;
                let term = horiz[sy * w + x] * weight;
                acc += term;
            }
            out[y * w + x] = acc;
        }
    }
    out
}

/// マスクブレンド。`out = before + (after - before) * w`(RGBA 4 チャンネル、固定順序)。
///
/// `before` / `after` は**同じ作業空間**の同寸法バッファであること
/// (エンジンが op の作業空間へ揃えてから `before` を退避している)。
///
/// **端点は式ではなく分岐で確定させる**: `w == 1.0` なら `after` を、`w == 0.0` なら
/// `before` を**そのまま**返す。f32 では `x + (y - x) * 1.0` が `y` と一致しない
/// ことがある(例: `0.5 + (0.1 - 0.5) = 0.099999994`)ため、式のままでは
/// 「全白マスク = マスク無し」「全黒マスク = 恒等」がバイト同一にならない。
/// この分岐は端点の意味論を保証するためのもので、中間値の計算には影響しない。
pub fn blend(before: &LinearImage, after: &LinearImage, weights: &[f32]) -> LinearImage {
    debug_assert_eq!(before.dimensions(), after.dimensions());
    debug_assert_eq!(weights.len(), after.data.len());
    let mut out = after.clone();
    for ((px, base), &w) in out.data.iter_mut().zip(before.data.iter()).zip(weights) {
        if w == 1.0 {
            continue;
        }
        if w == 0.0 {
            *px = *base;
            continue;
        }
        for c in 0..4 {
            let delta = px[c] - base[c];
            let step = delta * w;
            px[c] = base[c] + step;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blend_endpoints_are_exact() {
        let before = LinearImage::from_pixel(2, 1, [0.25, 0.5, 0.75, 1.0]);
        let mut after = LinearImage::from_pixel(2, 1, [0.9, 0.1, 0.2, 0.5]);
        after.set(0, 0, [0.9, 0.1, 0.2, 0.5]);
        let out = blend(&before, &after, &[1.0, 0.0]);
        assert_eq!(out.get(0, 0), after.get(0, 0));
        assert_eq!(out.get(1, 0), before.get(1, 0));
    }

    #[test]
    fn bilinear_upscale_is_monotone() {
        // 2x1 の [0, 1] を 4x1 へ広げると単調増加になる。
        let out = bilinear_resize(&[0.0, 1.0], 2, 1, 4, 1);
        for pair in out.windows(2) {
            assert!(pair[1] >= pair[0], "{out:?}");
        }
        assert_eq!(out[0], 0.0);
        assert_eq!(out[3], 1.0);
    }

    #[test]
    fn bilinear_same_size_is_identity() {
        let src = vec![0.1f32, 0.4, 0.9, 0.25];
        assert_eq!(bilinear_resize(&src, 2, 2, 2, 2), src);
    }
}
