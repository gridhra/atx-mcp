//! 3D LUT(.cube)適用。
//!
//! - パース: Adobe Cube LUT Specification 1.0(LUT_1D_SIZE / LUT_3D_SIZE、
//!   DOMAIN_MIN/MAX、TITLE、# コメント)に従う
//! - 補間: 3D は四面体補間(業界標準・回転対称性が良い)、1D は線形補間
//! - 決定論: LUT 値は f64 でパース後 1e-6 グリッドへ量子化。補間は固定順序の四則演算
//! - strength: 出力 = lerp(元, LUT適用後, strength) を f64 で計算し half-away-from-zero 丸め

use image::RgbaImage;

use crate::{AtxError, Result};

/// 3D LUT の格子サイズ上限(仕様上は 2..=256 だが、メモリ実効性から 129 に制限)。
const MAX_3D_SIZE: u32 = 129;
/// 1D LUT のエントリ数上限(仕様値)。
const MAX_1D_SIZE: u32 = 65536;
/// データ値の許容範囲。domain 外の僅かなはみ出し(丸め誤差・意図的なオーバーレンジ)は
/// 受け入れ、適用時にクランプする。
const DATA_SLACK: f64 = 16.0;

/// パース済み LUT(1D または 3D)。
pub struct CubeLut {
    /// 3D なら size^3 * 3、1D なら size * 3 の f64(量子化済み、R,G,B の順)。
    pub data: Vec<f64>,
    pub size: u32,
    pub is_3d: bool,
    pub domain_min: [f64; 3],
    pub domain_max: [f64; 3],
}

impl CubeLut {
    /// 3D 格子点 (r, g, b) の値を返す。データ順は仕様どおり red が最速。
    #[inline]
    fn node3(&self, r: u32, g: u32, b: u32) -> [f64; 3] {
        let n = self.size as usize;
        let idx = ((b as usize * n + g as usize) * n + r as usize) * 3;
        [self.data[idx], self.data[idx + 1], self.data[idx + 2]]
    }
}

pub fn validate(index: usize, lut_revision_id: &str, strength: f64) -> Result<()> {
    if lut_revision_id.is_empty() {
        return Err(AtxError::InvalidRecipe(format!(
            "operations[{index}] (lut): lut_revision_id must not be empty"
        )));
    }
    if !lut_revision_id.starts_with("rev_") {
        return Err(AtxError::InvalidRecipe(format!(
            "operations[{index}] (lut): lut_revision_id must start with \"rev_\", \
             got {lut_revision_id:?}"
        )));
    }
    if !strength.is_finite() || !(0.0..=1.0).contains(&strength) {
        return Err(AtxError::InvalidRecipe(format!(
            "operations[{index}] (lut): strength must be within 0.0..=1.0, got {strength}"
        )));
    }
    Ok(())
}

/// 行番号(1 始まり)付きのパースエラー。
fn err(line: usize, reason: impl std::fmt::Display) -> AtxError {
    AtxError::InvalidRecipe(format!("invalid .cube LUT at line {line}: {reason}"))
}

/// 3 つの浮動小数をパースする(キーワード行・データ行共通)。
fn parse_triple(fields: &[&str], line: usize, what: &str) -> Result<[f64; 3]> {
    if fields.len() != 3 {
        return Err(err(
            line,
            format!("{what} expects 3 numbers, got {}", fields.len()),
        ));
    }
    let mut out = [0.0f64; 3];
    for (i, f) in fields.iter().enumerate() {
        let v: f64 = f
            .parse()
            .map_err(|_| err(line, format!("{what}: {f:?} is not a number")))?;
        if !v.is_finite() {
            return Err(err(line, format!("{what}: {f:?} is not finite")));
        }
        out[i] = quantize(v);
    }
    Ok(out)
}

/// f64 を 1e-6 グリッドへ量子化する(決定論規約。mod.rs 参照)。
fn quantize(v: f64) -> f64 {
    (v * 1e6).round() / 1e6
}

/// half-away-from-zero で f64 を u8(0..=255)へ丸める。
/// ops::blur 等と同じ規約だが、モジュール間結合を避けるためここで独自に持つ。
fn round_to_u8(v: f64) -> u8 {
    let r = if v >= 0.0 {
        (v + 0.5).floor()
    } else {
        (v - 0.5).ceil()
    };
    r.clamp(0.0, 255.0) as u8
}

/// .cube テキストをパースする。エラーは行番号付き。
pub fn parse_cube(text: &str) -> Result<CubeLut> {
    let mut size: Option<(u32, bool)> = None; // (size, is_3d)
    let mut domain_min = [0.0f64; 3];
    let mut domain_max = [1.0f64; 3];
    let mut saw_domain_min = false;
    let mut saw_domain_max = false;
    let mut data: Vec<f64> = Vec::new();

    for (i, raw) in text.lines().enumerate() {
        let line = i + 1;
        // BOM とインラインコメントを落とす。
        let body = raw.trim_start_matches('\u{feff}');
        let body = match body.find('#') {
            Some(p) => &body[..p],
            None => body,
        };
        let body = body.trim();
        if body.is_empty() {
            continue;
        }
        let mut fields = body.split_whitespace();
        let keyword = fields.next().unwrap_or("");
        let rest: Vec<&str> = fields.collect();

        match keyword.to_ascii_uppercase().as_str() {
            "TITLE" => {
                // TITLE は表示用メタデータ。内容は使わないが、体裁だけ確認する。
                if rest.is_empty() {
                    return Err(err(line, "TITLE expects a quoted string"));
                }
            }
            "LUT_3D_SIZE" | "LUT_1D_SIZE" => {
                if size.is_some() {
                    return Err(err(
                        line,
                        "duplicate LUT size line (exactly one of LUT_1D_SIZE / LUT_3D_SIZE \
                         is allowed)",
                    ));
                }
                if !data.is_empty() {
                    return Err(err(line, "LUT size line must come before the data lines"));
                }
                let is_3d = keyword.eq_ignore_ascii_case("LUT_3D_SIZE");
                if rest.len() != 1 {
                    return Err(err(line, format!("{keyword} expects exactly 1 integer")));
                }
                let n: u32 = rest[0].parse().map_err(|_| {
                    err(line, format!("{keyword}: {:?} is not an integer", rest[0]))
                })?;
                let max = if is_3d { MAX_3D_SIZE } else { MAX_1D_SIZE };
                if !(2..=max).contains(&n) {
                    return Err(err(
                        line,
                        format!("{keyword} must be within 2..={max}, got {n}"),
                    ));
                }
                size = Some((n, is_3d));
            }
            "DOMAIN_MIN" | "DOMAIN_MAX" => {
                let is_min = keyword.eq_ignore_ascii_case("DOMAIN_MIN");
                if (is_min && saw_domain_min) || (!is_min && saw_domain_max) {
                    return Err(err(line, format!("duplicate {keyword}")));
                }
                let v = parse_triple(&rest, line, keyword)?;
                if is_min {
                    domain_min = v;
                    saw_domain_min = true;
                } else {
                    domain_max = v;
                    saw_domain_max = true;
                }
                for ch in 0..3 {
                    if domain_min[ch] >= domain_max[ch] {
                        return Err(err(
                            line,
                            format!(
                                "DOMAIN_MIN must be smaller than DOMAIN_MAX per channel \
                                 (channel {ch}: {} >= {})",
                                domain_min[ch], domain_max[ch]
                            ),
                        ));
                    }
                }
            }
            _ => {
                // データ行(先頭が数値)。
                let Some((n, is_3d)) = size else {
                    return Err(err(
                        line,
                        "data line appears before LUT_1D_SIZE / LUT_3D_SIZE",
                    ));
                };
                let mut fields = Vec::with_capacity(3);
                fields.push(keyword);
                fields.extend_from_slice(&rest);
                let v = parse_triple(&fields, line, "data line")?;
                for c in v {
                    if !(-DATA_SLACK..=DATA_SLACK).contains(&c) {
                        return Err(err(
                            line,
                            format!("data value {c} is far outside the expected 0..=1 range"),
                        ));
                    }
                }
                let expected = expected_entries(n, is_3d);
                if data.len() / 3 >= expected {
                    return Err(err(
                        line,
                        format!("too many data lines (expected exactly {expected})"),
                    ));
                }
                data.extend_from_slice(&v);
            }
        }
    }

    let Some((size, is_3d)) = size else {
        return Err(AtxError::InvalidRecipe(
            "invalid .cube LUT: missing LUT_1D_SIZE / LUT_3D_SIZE line".to_string(),
        ));
    };
    let expected = expected_entries(size, is_3d);
    if data.len() / 3 != expected {
        return Err(AtxError::InvalidRecipe(format!(
            "invalid .cube LUT: expected {expected} data lines, got {}",
            data.len() / 3
        )));
    }

    Ok(CubeLut {
        data,
        size,
        is_3d,
        domain_min,
        domain_max,
    })
}

/// 期待されるデータ行数。3D は size^3、1D は size。
fn expected_entries(size: u32, is_3d: bool) -> usize {
    let n = size as usize;
    if is_3d {
        n * n * n
    } else {
        n
    }
}

pub fn apply(img: &RgbaImage, lut: &CubeLut, strength: f64) -> RgbaImage {
    let mut out = img.clone();
    if strength == 0.0 {
        return out;
    }
    for px in out.pixels_mut() {
        let orig = [px[0] as f64, px[1] as f64, px[2] as f64];
        // domain 正規化(0..1 へクランプ)。
        let mut v01 = [0.0f64; 3];
        for ch in 0..3 {
            let min = lut.domain_min[ch];
            let max = lut.domain_max[ch];
            let t = (orig[ch] / 255.0 - min) / (max - min);
            v01[ch] = t.clamp(0.0, 1.0);
        }
        let mapped = if lut.is_3d {
            sample_3d(lut, v01)
        } else {
            sample_1d(lut, v01)
        };
        for ch in 0..3 {
            let target = (mapped[ch].clamp(0.0, 1.0)) * 255.0;
            px[ch] = round_to_u8(orig[ch] + (target - orig[ch]) * strength);
        }
        // アルファは不変。
    }
    out
}

/// 1D LUT: チャンネルごとの線形補間。
fn sample_1d(lut: &CubeLut, v01: [f64; 3]) -> [f64; 3] {
    let last = (lut.size - 1) as f64;
    let mut out = [0.0f64; 3];
    for ch in 0..3 {
        let pos = v01[ch] * last;
        let i0 = (pos.floor() as u32).min(lut.size - 1);
        let i1 = (i0 + 1).min(lut.size - 1);
        let f = pos - i0 as f64;
        let a = lut.data[i0 as usize * 3 + ch];
        let b = lut.data[i1 as usize * 3 + ch];
        out[ch] = a + (b - a) * f;
    }
    out
}

/// 3D LUT: 四面体補間。
///
/// 単位立方体を fr/fg/fb の大小関係で 6 個の四面体に分解し、
/// 「基準点 c000 から目的点 c111 へ至る 3 辺」の重み付き和で補間する
/// (三線形補間より格子点間の直線性が保たれ、グレー軸の色ズレが出にくい)。
/// 累算順序は固定(基準点 → 第1辺 → 第2辺 → 第3辺)。
fn sample_3d(lut: &CubeLut, v01: [f64; 3]) -> [f64; 3] {
    let n = lut.size;
    let last = (n - 1) as f64;

    let mut i0 = [0u32; 3];
    let mut i1 = [0u32; 3];
    let mut frac = [0.0f64; 3];
    for ch in 0..3 {
        let pos = v01[ch] * last;
        let f = pos.floor();
        let idx = (f as u32).min(n - 1);
        i0[ch] = idx;
        i1[ch] = (idx + 1).min(n - 1);
        frac[ch] = pos - f;
    }
    let (r0, g0, b0) = (i0[0], i0[1], i0[2]);
    let (r1, g1, b1) = (i1[0], i1[1], i1[2]);
    let (dr, dg, db) = (frac[0], frac[1], frac[2]);

    let c000 = lut.node3(r0, g0, b0);
    let c111 = lut.node3(r1, g1, b1);

    // (第1辺の終点, 第2辺の終点, 重み1, 重み2, 重み3)を選ぶ。
    let (p1, p2, w1, w2, w3) = if dr > dg {
        if dg > db {
            // dr > dg > db
            (lut.node3(r1, g0, b0), lut.node3(r1, g1, b0), dr, dg, db)
        } else if dr > db {
            // dr > db > dg
            (lut.node3(r1, g0, b0), lut.node3(r1, g0, b1), dr, db, dg)
        } else {
            // db > dr > dg
            (lut.node3(r0, g0, b1), lut.node3(r1, g0, b1), db, dr, dg)
        }
    } else if db > dg {
        // db > dg > dr
        (lut.node3(r0, g0, b1), lut.node3(r0, g1, b1), db, dg, dr)
    } else if db > dr {
        // dg > db > dr
        (lut.node3(r0, g1, b0), lut.node3(r0, g1, b1), dg, db, dr)
    } else {
        // dg > dr > db
        (lut.node3(r0, g1, b0), lut.node3(r1, g1, b0), dg, dr, db)
    };

    let mut out = [0.0f64; 3];
    for ch in 0..3 {
        out[ch] =
            c000[ch] + w1 * (p1[ch] - c000[ch]) + w2 * (p2[ch] - p1[ch]) + w3 * (c111[ch] - p2[ch]);
    }
    out
}
