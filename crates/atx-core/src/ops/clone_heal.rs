//! `clone`(クローンスタンプ)と `heal`(スポット修復)。v0.7、DESIGN.md §9.8。
//!
//! **作業空間: 線形光**(`ops/mod.rs` の表)。どちらも画素を混ぜる op
//! (クローンは縁のフェザで加重平均、ヒールは低周波と高周波の加算)なので、
//! 符号値のまま混ぜると縁が暗部へ沈む。
//!
//! # 適用領域は op が自前で持つ(マスク非対応)
//!
//! 両 op は `radius` + `feather_px` の**円**という適用領域を自分で持っている。
//! v0.5 の `mask` と併用できるようにすると「どちらが効くのか」が二重定義に
//! なるので、`Operation::mask()` はこの 2 op に対して常に `None` を返す。
//!
//! # フェザ重み
//!
//! 中心からの距離 `r` に対して
//!
//! ```text
//! inner = radius − min(feather_px, radius)
//! r > radius            → 0
//! r <= inner            → 1
//! それ以外              → t = (radius − r) / (radius − inner), w = t²(3 − 2t)
//! ```
//!
//! `t²(3 − 2t)`(smoothstep)は端点で 1 階微分が 0 になるので、縁に線形補間の
//! ような折れ目が出ない。重みは f64 で計算してから **1e-6 グリッドへ量子化**する
//! (`ops/mod.rs` の係数規約。`sqrt` は IEEE-754 で厳密に丸められる演算なので
//! 画素ループで呼んでよい — libm 依存の exp / pow とは扱いが違う)。
//!
//! **端点は式ではなく分岐で確定させる**(§9.7 と同じ規約): `w == 0` は書き込まず、
//! `w == 1` は補間式を通さずソース画素をそのまま代入する。
//! `b + (s − b) × 1.0` は f32 で `s` に戻るとは限らないため、
//! 「円の内側は厳密な複写」というテスト可能な性質はこの分岐が担保している。
//!
//! # スナップショット意味論(clone)
//!
//! 読み出しは**常に適用前の画像**から行い、書き込みは複製したバッファへ行う。
//! src と dest の円が重なっていても、複写した画素をさらに複写する
//! (= 尾を引く)ことは起きない。
//!
//! # heal のアルゴリズム(テクスチャ + トーン分解)
//!
//! ```text
//! detail = src_patch − gaussian_blur(src_patch, σ = radius/3)   ← 高周波(肌理)
//! tone   = gaussian_blur(dest_patch, σ = radius/3)              ← 低周波(明るさ・色)
//! healed = clamp(detail + tone, 0..1)
//! ```
//!
//! ソースからは**肌理だけ**、目的地からは**明るさと色かぶりだけ**を採るので、
//! 「クリーンな別領域から持ってきたのに周囲と明るさが違う」という clone の
//! 典型的な破綻が起きない。σ = radius/3 はガウスの半値幅がちょうど円の半径に
//! なる選び方(`blur` のカーネル半径 `ceil(3σ)` と一致する)。
//!
//! アルファも RGB と同じ式で処理する(`clone` の複写もアルファを含む)。
//! パッチのぼかしは既存の `ops::blur::gaussian_blur` をそのまま使うので、
//! 量子化カーネルとプリマルチプライ規約は blur と共有される。
//!
//! # 決定論
//!
//! 乱数・反復探索を一切使わない(PatchMatch は使っていない。DESIGN.md §9.8 参照)。
//! 走査順は dest 円の外接矩形を y → x の昇順で 1 回だけ。

use crate::linear::{quantize_1e6, LinearImage};
use crate::{AtxError, Result};

/// `radius` の上限。2048 なら 4097×4097 のパッチで、100MP の入力でも扱える。
const MAX_RADIUS: u32 = 2048;
/// `feather_px` の上限(`MaskRef::feather_px` と同じ 200px)。
const MAX_FEATHER_PX: f64 = 200.0;

/// `clone` / `heal` の静的検証(`op` はエラー文に出す op 名)。
pub fn validate(index: usize, op: &str, radius: u32, feather_px: f64) -> Result<()> {
    if !(1..=MAX_RADIUS).contains(&radius) {
        return Err(AtxError::InvalidRecipe(format!(
            "operations[{index}] ({op}): radius must be within 1..={MAX_RADIUS}, got {radius}"
        )));
    }
    if !feather_px.is_finite() || !(0.0..=MAX_FEATHER_PX).contains(&feather_px) {
        return Err(AtxError::InvalidRecipe(format!(
            "operations[{index}] ({op}): feather_px must be within 0.0..={MAX_FEATHER_PX}, \
             got {feather_px}"
        )));
    }
    Ok(())
}

/// 円の中心からのオフセット `(dx, dy)` に対するフェザ重み(0..1、1e-6 量子化)。
fn feather_weight(dx: i64, dy: i64, radius: u32, feather_px: f64) -> f64 {
    let r_sq = (dx * dx + dy * dy) as f64;
    let r = r_sq.sqrt();
    let big_r = radius as f64;
    if r > big_r {
        return 0.0;
    }
    let feather = if feather_px > big_r {
        big_r
    } else {
        feather_px
    };
    if feather <= 0.0 {
        return 1.0;
    }
    let inner = big_r - feather;
    if r <= inner {
        return 1.0;
    }
    // t は縁で 0、inner で 1。
    let t = (big_r - r) / feather;
    let t = t.clamp(0.0, 1.0);
    // smoothstep: t² × (3 − 2t)。乗算と加算は分けて書く(FMA 禁止の規約)。
    let two_t = 2.0 * t;
    let k = 3.0 - two_t;
    let t_sq = t * t;
    quantize_1e6(t_sq * k)
}

/// 中心が画像内にあることを確かめる。外れていれば構造化エラー用の文字列を返す。
fn check_centers(
    op: &str,
    img: &LinearImage,
    src_x: u32,
    src_y: u32,
    dest_x: u32,
    dest_y: u32,
) -> std::result::Result<(), String> {
    let (w, h) = img.dimensions();
    for (name, x, y) in [("src", src_x, src_y), ("dest", dest_x, dest_y)] {
        if x >= w || y >= h {
            return Err(format!(
                "{op}: the {name} center ({x}, {y}) is outside the current {w}x{h} image \
                 (centers must be inside the image; the circle itself may extend past the \
                 edge and is clipped). Adjust {name}_x/{name}_y to be within \
                 0..{} / 0..{}",
                w - 1,
                h - 1
            ));
        }
    }
    Ok(())
}

/// dest 円の外接矩形(画像内へクリップ済み)を `(x0, y0, x1, y1)`(端点含む)で返す。
/// 円が完全に画像外なら `None`(中心は画像内なので実際には起きないが、防御的に扱う)。
fn dest_bounds(
    img: &LinearImage,
    dest_x: u32,
    dest_y: u32,
    radius: u32,
) -> Option<(i64, i64, i64, i64)> {
    let (w, h) = img.dimensions();
    let r = radius as i64;
    let x0 = (dest_x as i64 - r).max(0);
    let y0 = (dest_y as i64 - r).max(0);
    let x1 = (dest_x as i64 + r).min(w as i64 - 1);
    let y1 = (dest_y as i64 + r).min(h as i64 - 1);
    if x0 > x1 || y0 > y1 {
        return None;
    }
    Some((x0, y0, x1, y1))
}

/// 重み `w` で `base`(dest 側)へ `value` を載せる(4 チャンネル、固定順)。
#[inline]
fn composite_pixel(base: [f32; 4], value: [f32; 4], w: f64) -> [f32; 4] {
    if w >= 1.0 {
        // 端点は式ではなく分岐で確定させる(モジュールコメント参照)。
        return value;
    }
    let wf = w as f32;
    let mut out = base;
    for c in 0..4 {
        let d = value[c] - base[c];
        let t = d * wf;
        out[c] = base[c] + t;
    }
    out
}

/// クローンスタンプ。`src` 中心の円を `dest` 中心へ複写する。
///
/// 読み出しは常に `img`(適用前のスナップショット)から行い、書き込みは複製した
/// バッファへ行う(src / dest の円が重なっても尾を引かない)。
/// src 側が画像外になる画素は複写しない(dest の元画素がそのまま残る)。
pub fn apply_clone(
    img: &LinearImage,
    src_x: u32,
    src_y: u32,
    dest_x: u32,
    dest_y: u32,
    radius: u32,
    feather_px: f64,
) -> std::result::Result<LinearImage, String> {
    check_centers("clone", img, src_x, src_y, dest_x, dest_y)?;
    let mut out = img.clone();
    let (w, h) = img.dimensions();
    let Some((x0, y0, x1, y1)) = dest_bounds(img, dest_x, dest_y, radius) else {
        return Ok(out);
    };

    for y in y0..=y1 {
        let dy = y - dest_y as i64;
        for x in x0..=x1 {
            let dx = x - dest_x as i64;
            let weight = feather_weight(dx, dy, radius, feather_px);
            if weight <= 0.0 {
                continue;
            }
            let sx = src_x as i64 + dx;
            let sy = src_y as i64 + dy;
            // src がはみ出す画素は「複写元が無い」ので何もしない(クリップ)。
            if sx < 0 || sy < 0 || sx >= w as i64 || sy >= h as i64 {
                continue;
            }
            let value = img.get(sx as u32, sy as u32);
            let base = img.get(x as u32, y as u32);
            out.set(x as u32, y as u32, composite_pixel(base, value, weight));
        }
    }
    Ok(out)
}

/// `center` を中心とする一辺 `2·radius+1` の正方パッチを取り出す。
///
/// 画像外は**端の画素へクランプ**して読む(`LinearImage::get_clamped`)。
/// こうすると src / dest のパッチが常に同じ寸法になり、`detail` と `tone` を
/// 添字どうしで足せる(「はみ出した分だけ切り詰める」と 2 つのパッチの
/// 対応がずれてしまう)。
fn extract_patch(img: &LinearImage, cx: u32, cy: u32, radius: u32) -> LinearImage {
    let side = 2 * radius + 1;
    let r = radius as i64;
    let mut patch = LinearImage::new(side, side);
    for py in 0..side {
        let sy = cy as i64 + py as i64 - r;
        for px in 0..side {
            let sx = cx as i64 + px as i64 - r;
            patch.set(px, py, img.get_clamped(sx, sy));
        }
    }
    patch
}

/// 画像の対角長(切り上げ、最低 1)。`heal` の実効半径の上限。
fn diagonal_cap(img: &LinearImage) -> u32 {
    let (w, h) = img.dimensions();
    let d = ((w as f64) * (w as f64) + (h as f64) * (h as f64))
        .sqrt()
        .ceil();
    (d as u32).max(1)
}

/// スポット修復。ソースの高周波 + 目的地の低周波を合成して円領域へ載せる。
/// アルゴリズムの根拠はモジュールコメントと DESIGN.md §9.8。
///
/// # 実効半径は画像の対角長で頭打ちにする
///
/// `heal` は一辺 `2·radius+1` の正方パッチを 2 枚取り出してぼかす。`radius` の
/// 上限 2048 をそのまま使うと 4097×4097×2 枚 = 1.6GB を確保することになり、
/// **8×8 の画像に radius 2048 を指定しただけでメモリを食い潰す**。
/// 対角長より大きい円は画像全体を覆い切っているので、実効半径をそこで打ち切っても
/// 「どの画素に効くか」は変わらない(フェザ帯の重み分布だけが、対角長を半径とした
/// 円のものになる)。打ち切りは黙って行う: `heal` の戻り値は画像だけで警告経路が
/// 無く、指定できる最大半径は静的検証の側で説明されているため。
///
/// # 大きな半径とカーネル上限(`blur` との相互作用)
///
/// 低周波の抽出は `ops::blur::gaussian_blur(σ = radius/3)` をそのまま使うが、
/// blur のカーネル半径は 255 で頭打ちになる。したがって **`radius > 765` では
/// σ に対してカーネルが足りず**、理想のガウスより裾の短い低域通過になる
/// (= テクスチャ + トーン分解が文書どおりの厳密な形からわずかにずれる)。
/// 実害は小さい: 打ち切られる裾の重みはごく僅かで、`detail` と `tone` の双方に
/// 同じカーネルが掛かるため誤差の大半は相殺する。それでも「radius を上げ続ければ
/// いくらでも滑らかなトーンになる」わけではないことは意識しておくこと。
pub fn apply_heal(
    img: &LinearImage,
    src_x: u32,
    src_y: u32,
    dest_x: u32,
    dest_y: u32,
    radius: u32,
    feather_px: f64,
) -> std::result::Result<LinearImage, String> {
    check_centers("heal", img, src_x, src_y, dest_x, dest_y)?;
    // 実効半径の頭打ち(上のドキュメント参照)。パッチ確保の前に行う。
    let radius = radius.min(diagonal_cap(img));
    let mut out = img.clone();
    let Some((x0, y0, x1, y1)) = dest_bounds(img, dest_x, dest_y, radius) else {
        return Ok(out);
    };

    // σ = radius/3 ⇔ blur のカーネル半径 ceil(3σ) が円の半径と一致する。
    let sigma = radius as f64 / 3.0;
    let src_patch = extract_patch(img, src_x, src_y, radius);
    let dest_patch = extract_patch(img, dest_x, dest_y, radius);
    let src_low = crate::ops::blur::gaussian_blur(&src_patch, sigma);
    let dest_low = crate::ops::blur::gaussian_blur(&dest_patch, sigma);

    let r = radius as i64;
    for y in y0..=y1 {
        let dy = y - dest_y as i64;
        for x in x0..=x1 {
            let dx = x - dest_x as i64;
            let weight = feather_weight(dx, dy, radius, feather_px);
            if weight <= 0.0 {
                continue;
            }
            // パッチ座標(必ずパッチ内: |dx|, |dy| <= radius)。
            let px = (dx + r) as u32;
            let py = (dy + r) as u32;
            let s = src_patch.get(px, py);
            let sl = src_low.get(px, py);
            let dl = dest_low.get(px, py);

            let mut healed = [0f32; 4];
            for (c, slot) in healed.iter_mut().enumerate() {
                // detail = src − blur(src) / tone = blur(dest)
                let detail = s[c] - sl[c];
                let v = detail + dl[c];
                *slot = v.clamp(0.0, 1.0);
            }
            let base = img.get(x as u32, y as u32);
            out.set(x as u32, y as u32, composite_pixel(base, healed, weight));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weight_is_binary_without_feather() {
        assert_eq!(feather_weight(0, 0, 10, 0.0), 1.0);
        assert_eq!(feather_weight(10, 0, 10, 0.0), 1.0); // r == radius は内側
        assert_eq!(feather_weight(11, 0, 10, 0.0), 0.0);
        assert_eq!(feather_weight(8, 8, 10, 0.0), 0.0); // r = 11.3 > 10
    }

    #[test]
    fn feather_band_is_monotonic_and_hits_both_endpoints() {
        let radius = 20u32;
        let feather = 6.0;
        let mut prev = 1.0;
        for d in 0..=25i64 {
            let w = feather_weight(d, 0, radius, feather);
            assert!(
                w <= prev + 1e-12,
                "weight must not increase outward at d={d}"
            );
            prev = w;
        }
        assert_eq!(feather_weight(14, 0, radius, feather), 1.0); // inner 境界
        assert_eq!(feather_weight(20, 0, radius, feather), 0.0); // 縁
        let mid = feather_weight(17, 0, radius, feather);
        assert!(mid > 0.0 && mid < 1.0, "mid band weight {mid}");
    }

    /// feather が radius より大きいときは radius へクランプされる(w(0) = 1 のまま)。
    #[test]
    fn oversized_feather_clamps_to_radius() {
        assert_eq!(feather_weight(0, 0, 5, 200.0), 1.0);
        assert_eq!(feather_weight(5, 0, 5, 200.0), 0.0);
    }

    #[test]
    fn clone_of_uniform_source_is_exact_inside_the_circle() {
        let mut img = LinearImage::from_pixel(32, 32, [0.1, 0.2, 0.3, 1.0]);
        for y in 0..8 {
            for x in 0..8 {
                img.set(x, y, [0.7, 0.6, 0.5, 1.0]);
            }
        }
        let out = apply_clone(&img, 3, 3, 20, 20, 3, 0.0).unwrap();
        assert_eq!(out.get(20, 20), [0.7, 0.6, 0.5, 1.0]);
        // 円の外は不変。
        assert_eq!(out.get(20, 25), [0.1, 0.2, 0.3, 1.0]);
    }

    #[test]
    fn center_outside_image_is_an_error() {
        let img = LinearImage::from_pixel(8, 8, [0.5, 0.5, 0.5, 1.0]);
        let err = apply_clone(&img, 9, 0, 0, 0, 2, 0.0).unwrap_err();
        assert!(err.contains("src center"), "{err}");
        let err = apply_heal(&img, 0, 0, 0, 9, 2, 0.0).unwrap_err();
        assert!(err.contains("dest center"), "{err}");
    }

    /// 回帰: 小さな画像に巨大な radius を渡してもメモリを食い潰さない。
    ///
    /// 以前は radius をそのままパッチの一辺(2r+1)に使っていたため、8x8 の画像 +
    /// radius 2048 で 4097x4097 の f32 RGBA パッチを 2 枚 = 1.6GB 確保していた。
    /// 実効半径を対角長(ceil)で頭打ちにすると、出力は「その半径で呼んだ場合」と
    /// **完全に一致**する。
    #[test]
    fn oversized_radius_is_capped_at_the_image_diagonal() {
        let mut img = LinearImage::from_pixel(8, 8, [0.3, 0.4, 0.5, 1.0]);
        img.set(2, 2, [0.9, 0.1, 0.2, 1.0]);
        img.set(5, 6, [0.0, 0.8, 0.4, 1.0]);

        let started = std::time::Instant::now();
        let huge = apply_heal(&img, 2, 2, 5, 5, MAX_RADIUS, 3.0).unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "capped heal must stay cheap, took {:?}",
            started.elapsed()
        );

        // ceil(sqrt(8² + 8²)) = 12。
        assert_eq!(diagonal_cap(&img), 12);
        let capped = apply_heal(&img, 2, 2, 5, 5, 12, 3.0).unwrap();
        assert_eq!(huge.data, capped.data);
    }

    /// 一様画像の heal は恒等(detail = 0、tone = 元の値)。
    #[test]
    fn heal_of_uniform_image_is_identity() {
        let img = LinearImage::from_pixel(24, 24, [0.4, 0.5, 0.6, 1.0]);
        let out = apply_heal(&img, 5, 5, 15, 15, 4, 0.0).unwrap();
        for (a, b) in out.data.iter().zip(img.data.iter()) {
            for c in 0..4 {
                assert!((a[c] - b[c]).abs() < 1e-6, "{a:?} vs {b:?}");
            }
        }
    }
}
