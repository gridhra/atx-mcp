//! `generate_mask` のマスク生成カーネル(v0.5 / ROADMAP Phase C)。
//!
//! ここは**純粋な画素計算**だけを持つ(ストア I/O は [`crate::tools`] 側)。
//! 生成物は「8bit グレースケール(luma を RGB 3チャンネルへ複製した PNG)」で、
//! `atx_core::recipe::MaskRef` の重み解釈(sRGB 符号値上の BT.709 輝度、
//! 白 = 1.0 = op を全量適用、黒 = 0.0 = 適用しない)とそのまま噛み合う。
//!
//! # 決定論(`atx_core::ops` §決定論の規約に準拠)
//!
//! - libm 由来の超越関数は**角度 → 方向ベクトルの `sin`/`cos` 1組だけ**で、
//!   結果は直後に 1e-6 グリッドへ量子化してから画素計算に入る。
//!   以降の per-pixel 計算は四則演算と `sqrt`(IEEE 754 で正しく丸められる)のみ。
//! - per-pixel の重みも 0..1 の 1e-6 グリッドへ量子化してから u8 化するので、
//!   丸め境界が環境依存の最下位ビットに乗ることがない。
//! - 総和は走査順そのままの左結合、`mul_add`(FMA)は使わない。
//!
//! したがって「同じ params + 同じ参照画像バイト列 → 同じ PNG バイト列」であり、
//! ストアの sha256 dedup がそのまま `generate_mask` の冪等性になる。

use image::RgbImage;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `generate_mask` の `kind` に指定できる値。
pub const MASK_KINDS: [&str; 4] = [
    "linear_gradient",
    "radial_gradient",
    "luminosity_range",
    "color_range",
];

/// 量子化グリッド(係数・重み)。`atx_core::ops` の規約と同じ 1e-6。
const GRID: f64 = 1e-6;

/// 1e-6 グリッドへの量子化。超越関数の結果と per-pixel の重みに必ず通す。
fn q6(v: f64) -> f64 {
    (v / GRID).round() * GRID
}

fn clamp01(v: f64) -> f64 {
    v.clamp(0.0, 1.0)
}

/// 0..1 の重みを 8bit のグレー値へ(half-away-from-zero 丸め)。
fn to_u8(w: f64) -> u8 {
    (clamp01(w) * 255.0).round() as u8
}

/// `lo` で 1.0、`hi` で 0.0 になる線形ランプ。`hi <= lo` のときは硬いステップ。
fn ramp_down(t: f64, lo: f64, hi: f64) -> f64 {
    if hi <= lo {
        if t <= lo {
            1.0
        } else {
            0.0
        }
    } else if t <= lo {
        1.0
    } else if t >= hi {
        0.0
    } else {
        (hi - t) / (hi - lo)
    }
}

// ---------------------------------------------------------------------------
// 引数
// ---------------------------------------------------------------------------

/// `generate_mask` の引数。
///
/// kind ごとに意味のあるフィールドだけを渡す(**フラットな任意フィールド**で、
/// 検証は kind ごとに行う)。kind に属さないフィールドを渡すと、
/// その kind が受け付けるフィールド名を列挙した構造化エラーになる。
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
pub struct GenerateMaskParams {
    /// 寸法(と、輝度/色域マスクでは画素)の供給元になる画像 revision ID。
    pub reference_revision_id: String,
    /// `"linear_gradient"` | `"radial_gradient"` | `"luminosity_range"` | `"color_range"`。
    pub kind: String,

    // -- linear_gradient ----------------------------------------------------
    /// linear_gradient: グラデーション軸の角度(度)。0 = 上が白で下へ向かって黒、
    /// 90 = 左が白で右へ向かって黒(正の角度で時計回り)。既定 0。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub angle_degrees: Option<f64>,
    /// linear_gradient: 軸上で重みが 1.0 のままでいる終端位置(0..1)。既定 0.0。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<f64>,
    /// linear_gradient: 軸上で重みが 0.0 に達する位置(0..1、`start` 以上)。既定 1.0。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<f64>,

    // -- radial_gradient ----------------------------------------------------
    /// radial_gradient: 中心の X(画像幅に対する相対値 0..1)。既定 0.5。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub center_x: Option<f64>,
    /// radial_gradient: 中心の Y(画像高に対する相対値 0..1)。既定 0.5。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub center_y: Option<f64>,
    /// radial_gradient: 重みが 1.0 の内円の半径(対角線の半分に対する比 0..1)。既定 0.5。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radius: Option<f64>,

    // -- feather(kind ごとに単位が違う共有フィールド)-----------------------
    /// 減衰帯の幅。radial_gradient では対角線の半分に対する比(0..1、既定 0.25)、
    /// luminosity_range では輝度単位(0..255、既定 16)、
    /// color_range では色相の度数(0..180、既定 15)。linear_gradient では使わない
    /// (`start`/`end` の間隔がフェザそのもの)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feather: Option<f64>,

    // -- luminosity_range ---------------------------------------------------
    /// luminosity_range: 完全に選択される輝度域の下限(0..255)。既定 0。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<u8>,
    /// luminosity_range: 完全に選択される輝度域の上限(0..255、`min` 以上)。既定 255。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<u8>,

    // -- color_range --------------------------------------------------------
    /// color_range: 中心色相(度、0..360)。必須。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hue_center: Option<f64>,
    /// color_range: 中心からの**片側**幅(度、1..180)。既定 30。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hue_width: Option<f64>,
}

/// 引数検証の失敗。[`crate::tools`] 側で構造化エラーへ写す。
#[derive(Debug, Clone)]
pub struct SpecError {
    pub code: &'static str,
    pub message: String,
    pub details: serde_json::Value,
}

impl SpecError {
    fn new(code: &'static str, message: impl Into<String>, details: serde_json::Value) -> Self {
        Self {
            code,
            message: message.into(),
            details,
        }
    }
}

fn out_of_range(field: &str, given: f64, range: &str) -> SpecError {
    SpecError::new(
        "invalid_mask_param",
        format!("{field} must be in {range}, got {given}"),
        serde_json::json!({
            "field": field,
            "given": given,
            "valid_range": range,
            "recovery": format!("call generate_mask again with {field} inside {range}"),
        }),
    )
}

fn check(field: &str, value: f64, lo: f64, hi: f64, range: &str) -> Result<f64, SpecError> {
    if !value.is_finite() || value < lo || value > hi {
        return Err(out_of_range(field, value, range));
    }
    Ok(value)
}

// ---------------------------------------------------------------------------
// 解決済みの仕様
// ---------------------------------------------------------------------------

/// 既定値まで解決済みのマスク仕様。
#[derive(Debug, Clone, PartialEq)]
pub enum MaskSpec {
    LinearGradient {
        angle_degrees: f64,
        start: f64,
        end: f64,
    },
    RadialGradient {
        center_x: f64,
        center_y: f64,
        radius: f64,
        feather: f64,
    },
    LuminosityRange {
        min: u8,
        max: u8,
        feather: f64,
    },
    ColorRange {
        hue_center: f64,
        hue_width: f64,
        feather: f64,
    },
}

/// kind ごとに受け付けるフィールド名(`reference_revision_id` / `kind` を除く)。
pub fn fields_for(kind: &str) -> &'static [&'static str] {
    match kind {
        "linear_gradient" => &["angle_degrees", "start", "end"],
        "radial_gradient" => &["center_x", "center_y", "radius", "feather"],
        "luminosity_range" => &["min", "max", "feather"],
        "color_range" => &["hue_center", "hue_width", "feather"],
        _ => &[],
    }
}

/// 実際に指定されたフィールド名(`reference_revision_id` / `kind` を除く)。
fn given_fields(p: &GenerateMaskParams) -> Vec<&'static str> {
    let mut given = Vec::new();
    let flags: [(&'static str, bool); 10] = [
        ("angle_degrees", p.angle_degrees.is_some()),
        ("start", p.start.is_some()),
        ("end", p.end.is_some()),
        ("center_x", p.center_x.is_some()),
        ("center_y", p.center_y.is_some()),
        ("radius", p.radius.is_some()),
        ("feather", p.feather.is_some()),
        ("min", p.min.is_some()),
        ("max", p.max.is_some()),
        ("hue_center", p.hue_center.is_some()),
    ];
    for (name, present) in flags {
        if present {
            given.push(name);
        }
    }
    if p.hue_width.is_some() {
        given.push("hue_width");
    }
    given
}

/// 引数を検証し、既定値まで解決した [`MaskSpec`] を返す。
pub fn build(p: &GenerateMaskParams) -> Result<MaskSpec, SpecError> {
    let kind = p.kind.trim();
    if !MASK_KINDS.contains(&kind) {
        return Err(SpecError::new(
            "invalid_mask_kind",
            format!("unknown mask kind {kind:?}; valid kinds are {MASK_KINDS:?}"),
            serde_json::json!({
                "given": p.kind,
                "valid_values": MASK_KINDS,
                "recovery": "call generate_mask again with one of valid_values",
            }),
        ));
    }

    // kind に属さないフィールドは黙って無視せず、その kind の語彙を示して弾く。
    let accepted = fields_for(kind);
    let unexpected: Vec<&'static str> = given_fields(p)
        .into_iter()
        .filter(|f| !accepted.contains(f))
        .collect();
    if !unexpected.is_empty() {
        return Err(SpecError::new(
            "unexpected_mask_param",
            format!("kind {kind:?} does not accept {unexpected:?}; it accepts {accepted:?}",),
            serde_json::json!({
                "kind": kind,
                "unexpected": unexpected,
                "accepted": accepted,
                "recovery": "drop the fields that belong to another kind, or change kind",
            }),
        ));
    }

    match kind {
        "linear_gradient" => {
            let angle_degrees = check(
                "angle_degrees",
                p.angle_degrees.unwrap_or(0.0),
                -360.0,
                360.0,
                "-360..360",
            )?;
            let start = check("start", p.start.unwrap_or(0.0), 0.0, 1.0, "0..1")?;
            let end = check("end", p.end.unwrap_or(1.0), 0.0, 1.0, "0..1")?;
            if end < start {
                return Err(SpecError::new(
                    "invalid_mask_param",
                    format!("end ({end}) must be >= start ({start})"),
                    serde_json::json!({
                        "start": start,
                        "end": end,
                        "recovery": "start is where the weight is still 1.0 and end is where it reaches 0.0, so end must not be below start",
                    }),
                ));
            }
            Ok(MaskSpec::LinearGradient {
                angle_degrees,
                start,
                end,
            })
        }
        "radial_gradient" => Ok(MaskSpec::RadialGradient {
            center_x: check("center_x", p.center_x.unwrap_or(0.5), 0.0, 1.0, "0..1")?,
            center_y: check("center_y", p.center_y.unwrap_or(0.5), 0.0, 1.0, "0..1")?,
            radius: check("radius", p.radius.unwrap_or(0.5), 0.0, 1.0, "0..1")?,
            feather: check("feather", p.feather.unwrap_or(0.25), 0.0, 1.0, "0..1")?,
        }),
        "luminosity_range" => {
            let min = p.min.unwrap_or(0);
            let max = p.max.unwrap_or(255);
            if max < min {
                return Err(SpecError::new(
                    "invalid_mask_param",
                    format!("max ({max}) must be >= min ({min})"),
                    serde_json::json!({
                        "min": min,
                        "max": max,
                        "recovery": "min and max bound the luminance band that is fully selected, so max must not be below min",
                    }),
                ));
            }
            Ok(MaskSpec::LuminosityRange {
                min,
                max,
                feather: check("feather", p.feather.unwrap_or(16.0), 0.0, 255.0, "0..255")?,
            })
        }
        "color_range" => {
            let hue_center = match p.hue_center {
                Some(v) => check("hue_center", v, 0.0, 360.0, "0..360")?,
                None => {
                    return Err(SpecError::new(
                        "missing_mask_param",
                        "kind \"color_range\" requires hue_center (degrees, 0..360)",
                        serde_json::json!({
                            "kind": "color_range",
                            "missing": "hue_center",
                            "accepted": fields_for("color_range"),
                            "recovery": "pass hue_center: the hue at the centre of the band (0=red, 60=yellow, 120=green, 240=blue)",
                        }),
                    ))
                }
            };
            Ok(MaskSpec::ColorRange {
                hue_center,
                hue_width: check(
                    "hue_width",
                    p.hue_width.unwrap_or(30.0),
                    1.0,
                    180.0,
                    "1..180",
                )?,
                feather: check("feather", p.feather.unwrap_or(15.0), 0.0, 180.0, "0..180")?,
            })
        }
        _ => unreachable!("kind was validated above"),
    }
}

impl MaskSpec {
    /// `kind` 文字列。
    pub fn kind(&self) -> &'static str {
        match self {
            MaskSpec::LinearGradient { .. } => "linear_gradient",
            MaskSpec::RadialGradient { .. } => "radial_gradient",
            MaskSpec::LuminosityRange { .. } => "luminosity_range",
            MaskSpec::ColorRange { .. } => "color_range",
        }
    }

    /// 参照画像の**画素**を読むか(false なら寸法しか使わない)。
    pub fn reads_reference_pixels(&self) -> bool {
        matches!(
            self,
            MaskSpec::LuminosityRange { .. } | MaskSpec::ColorRange { .. }
        )
    }

    /// origin の `generator` に載せる正規化 JSON。
    ///
    /// `serde_json::Map` は(`preserve_order` 無効の既定で)キー順が確定するので、
    /// 同じ仕様は必ず同じ文字列になる。
    pub fn canonical_json(&self) -> String {
        let mut map = serde_json::Map::new();
        map.insert("kind".into(), self.kind().into());
        match *self {
            MaskSpec::LinearGradient {
                angle_degrees,
                start,
                end,
            } => {
                map.insert("angle_degrees".into(), angle_degrees.into());
                map.insert("start".into(), start.into());
                map.insert("end".into(), end.into());
            }
            MaskSpec::RadialGradient {
                center_x,
                center_y,
                radius,
                feather,
            } => {
                map.insert("center_x".into(), center_x.into());
                map.insert("center_y".into(), center_y.into());
                map.insert("radius".into(), radius.into());
                map.insert("feather".into(), feather.into());
            }
            MaskSpec::LuminosityRange { min, max, feather } => {
                map.insert("min".into(), min.into());
                map.insert("max".into(), max.into());
                map.insert("feather".into(), feather.into());
            }
            MaskSpec::ColorRange {
                hue_center,
                hue_width,
                feather,
            } => {
                map.insert("hue_center".into(), hue_center.into());
                map.insert("hue_width".into(), hue_width.into());
                map.insert("feather".into(), feather.into());
            }
        }
        serde_json::Value::Object(map).to_string()
    }

    /// マスクを描画する。返るのは luma を 3チャンネルへ複製した RGB8 画像で、
    /// 寸法は `reference` と厳密に一致する。
    pub fn render(&self, reference: &RgbImage) -> RgbImage {
        let (w, h) = reference.dimensions();
        let mut out = RgbImage::new(w, h);
        match *self {
            MaskSpec::LinearGradient {
                angle_degrees,
                start,
                end,
            } => {
                // 唯一の超越関数。結果は直後に 1e-6 グリッドへ量子化して以降の
                // 画素計算に libm の最下位ビット差を持ち込まない。
                let radians = angle_degrees.to_radians();
                let dx = q6(radians.sin());
                let dy = q6(radians.cos());
                // 単位正方形上でこの軸に射影したときの全幅(角の寄与の和)。
                // これで割ると t は必ず 0..1 に収まる。
                let extent = dx.abs() + dy.abs();
                let extent = if extent < 1e-9 { 1.0 } else { extent };
                for y in 0..h {
                    let yn = (y as f64 + 0.5) / h as f64 - 0.5;
                    for x in 0..w {
                        let xn = (x as f64 + 0.5) / w as f64 - 0.5;
                        let projected = xn * dx + yn * dy;
                        let t = 0.5 + projected / extent;
                        let weight = q6(ramp_down(t, start, end));
                        put_gray(&mut out, x, y, to_u8(weight));
                    }
                }
            }
            MaskSpec::RadialGradient {
                center_x,
                center_y,
                radius,
                feather,
            } => {
                let cx = center_x * w as f64;
                let cy = center_y * h as f64;
                // 半対角。sqrt は IEEE 754 で正しく丸められるので環境差は出ないが、
                // 規約どおり量子化してから使う。
                let half_diag = q6(0.5 * ((w as f64 * w as f64) + (h as f64 * h as f64)).sqrt());
                let half_diag = if half_diag < 1e-9 { 1.0 } else { half_diag };
                for y in 0..h {
                    let py = y as f64 + 0.5 - cy;
                    for x in 0..w {
                        let px = x as f64 + 0.5 - cx;
                        let dist = ((px * px) + (py * py)).sqrt();
                        let t = q6(dist / half_diag);
                        let weight = q6(ramp_down(t, radius, radius + feather));
                        put_gray(&mut out, x, y, to_u8(weight));
                    }
                }
            }
            MaskSpec::LuminosityRange { min, max, feather } => {
                let lo = min as f64;
                let hi = max as f64;
                for y in 0..h {
                    for x in 0..w {
                        let p = reference.get_pixel(x, y).0;
                        let luma = luma709(p);
                        // 帯の内側は 1.0、外側は feather 幅で 0 へ落とす(両肩)。
                        let weight = if luma < lo {
                            ramp_down(lo - luma, 0.0, feather)
                        } else if luma > hi {
                            ramp_down(luma - hi, 0.0, feather)
                        } else {
                            1.0
                        };
                        put_gray(&mut out, x, y, to_u8(q6(weight)));
                    }
                }
            }
            MaskSpec::ColorRange {
                hue_center,
                hue_width,
                feather,
            } => {
                for y in 0..h {
                    for x in 0..w {
                        let p = reference.get_pixel(x, y).0;
                        let (hue, saturation) = hue_saturation(p);
                        let distance = hue_distance(hue, hue_center);
                        let band = ramp_down(distance, hue_width, hue_width + feather);
                        // 無彩色の画素は色相が定義できないので、彩度で滑らかに落とす。
                        let gate = clamp01(saturation / SATURATION_GATE);
                        put_gray(&mut out, x, y, to_u8(q6(band * gate)));
                    }
                }
            }
        }
        out
    }
}

/// 色相マスクで「色がある」とみなす彩度の下限(HSV 彩度)。これ未満は線形に減衰する。
const SATURATION_GATE: f64 = 0.10;

fn put_gray(out: &mut RgbImage, x: u32, y: u32, v: u8) {
    out.put_pixel(x, y, image::Rgb([v, v, v]));
}

/// sRGB 符号値上の BT.709 輝度(0..255)。左結合の固定順で足す。
fn luma709(p: [u8; 3]) -> f64 {
    let r = 0.2126 * p[0] as f64;
    let g = 0.7152 * p[1] as f64;
    let b = 0.0722 * p[2] as f64;
    (r + g) + b
}

/// HSV の色相(度、0..360)と彩度(0..1)。四則演算のみで求まる。
fn hue_saturation(p: [u8; 3]) -> (f64, f64) {
    let r = p[0] as f64 / 255.0;
    let g = p[1] as f64 / 255.0;
    let b = p[2] as f64 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    if delta <= 0.0 || max <= 0.0 {
        return (0.0, 0.0);
    }
    let hue = if max == r {
        60.0 * (((g - b) / delta) % 6.0)
    } else if max == g {
        60.0 * (((b - r) / delta) + 2.0)
    } else {
        60.0 * (((r - g) / delta) + 4.0)
    };
    let hue = if hue < 0.0 { hue + 360.0 } else { hue };
    (hue, delta / max)
}

/// 色相環上の最短距離(度、0..180)。
fn hue_distance(a: f64, b: f64) -> f64 {
    let d = (a - b).abs() % 360.0;
    if d > 180.0 {
        360.0 - d
    } else {
        d
    }
}

/// マスクの平均重み(0..1)。カバー率の目安として structuredContent に載せる。
pub fn mean_weight(mask: &RgbImage) -> f64 {
    let (w, h) = mask.dimensions();
    let count = (w as u64) * (h as u64);
    if count == 0 {
        return 0.0;
    }
    let mut sum: u64 = 0;
    for y in 0..h {
        for x in 0..w {
            sum += mask.get_pixel(x, y).0[0] as u64;
        }
    }
    q6(sum as f64 / (count as f64 * 255.0))
}

/// PNG(8bit RGB)へエンコードする。同じ画素なら必ず同じバイト列になる。
pub fn encode_png(mask: &RgbImage) -> Result<Vec<u8>, String> {
    use image::ImageEncoder;
    let (w, h) = mask.dimensions();
    let mut buf = Vec::new();
    image::codecs::png::PngEncoder::new(&mut buf)
        .write_image(mask.as_raw(), w, h, image::ExtendedColorType::Rgb8)
        .map_err(|e| e.to_string())?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(kind: &str) -> GenerateMaskParams {
        GenerateMaskParams {
            reference_revision_id: "rev_x".into(),
            kind: kind.into(),
            ..Default::default()
        }
    }

    fn flat(w: u32, h: u32, rgb: [u8; 3]) -> RgbImage {
        RgbImage::from_pixel(w, h, image::Rgb(rgb))
    }

    #[test]
    fn linear_gradient_at_zero_degrees_is_white_on_top() {
        let spec = build(&params("linear_gradient")).unwrap();
        let mask = spec.render(&flat(4, 64, [0, 0, 0]));
        let top = mask.get_pixel(0, 0).0[0];
        let bottom = mask.get_pixel(0, 63).0[0];
        assert!(top > 240, "top must be near white, got {top}");
        assert!(bottom < 15, "bottom must be near black, got {bottom}");
        // 横方向には変化しない。
        assert_eq!(mask.get_pixel(0, 10).0[0], mask.get_pixel(3, 10).0[0]);
    }

    #[test]
    fn linear_gradient_at_ninety_degrees_is_white_on_the_left() {
        let mut p = params("linear_gradient");
        p.angle_degrees = Some(90.0);
        let spec = build(&p).unwrap();
        let mask = spec.render(&flat(64, 4, [0, 0, 0]));
        assert!(mask.get_pixel(0, 0).0[0] > 240);
        assert!(mask.get_pixel(63, 0).0[0] < 15);
    }

    #[test]
    fn radial_gradient_is_white_at_the_centre_and_black_in_the_corners() {
        let mut p = params("radial_gradient");
        p.radius = Some(0.25);
        p.feather = Some(0.1);
        let spec = build(&p).unwrap();
        let mask = spec.render(&flat(64, 64, [0, 0, 0]));
        assert_eq!(mask.get_pixel(32, 32).0[0], 255);
        assert_eq!(mask.get_pixel(0, 0).0[0], 0);
    }

    #[test]
    fn luminosity_range_selects_only_the_band() {
        let mut p = params("luminosity_range");
        p.min = Some(200);
        p.max = Some(255);
        p.feather = Some(0.0);
        let spec = build(&p).unwrap();
        assert_eq!(
            spec.render(&flat(2, 2, [255, 255, 255])).get_pixel(0, 0).0[0],
            255
        );
        assert_eq!(spec.render(&flat(2, 2, [0, 0, 0])).get_pixel(0, 0).0[0], 0);
    }

    #[test]
    fn color_range_selects_the_hue_band_and_ignores_grey() {
        let mut p = params("color_range");
        p.hue_center = Some(240.0);
        p.hue_width = Some(30.0);
        p.feather = Some(0.0);
        let spec = build(&p).unwrap();
        assert_eq!(
            spec.render(&flat(2, 2, [0, 0, 255])).get_pixel(0, 0).0[0],
            255
        );
        assert_eq!(
            spec.render(&flat(2, 2, [255, 0, 0])).get_pixel(0, 0).0[0],
            0
        );
        // 無彩色は色相が定義できないので選択されない。
        assert_eq!(
            spec.render(&flat(2, 2, [128, 128, 128])).get_pixel(0, 0).0[0],
            0
        );
    }

    #[test]
    fn rendering_is_bit_identical_across_runs() {
        let mut p = params("linear_gradient");
        p.angle_degrees = Some(37.5);
        let spec = build(&p).unwrap();
        let reference = flat(23, 17, [10, 20, 30]);
        let a = encode_png(&spec.render(&reference)).unwrap();
        let b = encode_png(&spec.render(&reference)).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn canonical_json_is_stable_and_kind_scoped() {
        let spec = build(&params("linear_gradient")).unwrap();
        assert_eq!(
            spec.canonical_json(),
            r#"{"angle_degrees":0.0,"end":1.0,"kind":"linear_gradient","start":0.0}"#
        );
    }

    #[test]
    fn fields_from_another_kind_are_rejected() {
        let mut p = params("linear_gradient");
        p.radius = Some(0.5);
        let err = build(&p).unwrap_err();
        assert_eq!(err.code, "unexpected_mask_param");
    }

    #[test]
    fn color_range_requires_a_hue_centre_and_unknown_kinds_list_the_valid_ones() {
        assert_eq!(
            build(&params("color_range")).unwrap_err().code,
            "missing_mask_param"
        );
        assert_eq!(
            build(&params("nope")).unwrap_err().code,
            "invalid_mask_kind"
        );
    }

    #[test]
    fn ranges_are_validated_rather_than_clamped() {
        let mut p = params("radial_gradient");
        p.radius = Some(1.5);
        assert_eq!(build(&p).unwrap_err().code, "invalid_mask_param");

        let mut p = params("linear_gradient");
        p.start = Some(0.8);
        p.end = Some(0.2);
        assert_eq!(build(&p).unwrap_err().code, "invalid_mask_param");
    }
}
