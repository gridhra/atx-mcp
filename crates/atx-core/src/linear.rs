//! 画素エンジン v2 の内部表現: **f32 リニアライト** の RGBA バッファと、
//! sRGB 符号値 ⇄ 線形光の相互変換(量子化 LUT + 線形補間)。
//!
//! v1 では中間表現が `image::RgbaImage`(RGBA8・sRGB 符号値)だったため、
//! op を重ねるたびに 256 段へ丸め込まれ(ポスタリゼーション)、
//! リサイズ・ぼかしの平均も「符号値の平均」= 物理的に誤った暗部寄りの結果になっていた。
//! v2 はパイプライン中を通して f32 を保ち、幾何・フィルタ系は**線形光**で、
//! トーン・カラー系は**sRGB 符号値**で処理する(§ops/mod.rs の作業空間表)。
//!
//! # 決定論の規約(tests/f32_spike.rs の契約をそのまま踏襲)
//!
//! 1. **超越関数は LUT 構築時にのみ f64 で呼ぶ**。画素ループには一切持ち込まない。
//!    LUT の値は 1e-9 グリッドへ量子化してから f32 化するので、libm の数 ULP 差
//!    (相対 1e-15 程度)は同じ格子点に吸収される。
//! 2. **FMA 禁止**: `f32::mul_add` を使わず、乗算と加算を別の文/式に分ける。
//! 3. **再結合禁止**: 総和は走査順そのままの左結合で書く。
//!
//! # 変換 LUT の構成と精度
//!
//! - `EOTF_U8`(256 エントリ): u8 sRGB → 線形。デコード専用の厳密テーブル。
//! - `EOTF_U16`(65536 エントリ): u16 sRGB → 線形。PNG16 入力でのみ遅延構築する。
//! - `EOTF_F32`(4096 エントリ + 線形補間): sRGB f32 → 線形。
//!   sRGB 空間 op から線形空間へ戻る経路で使う。
//! - `OETF_F32`(4096 エントリ + 線形補間): 線形 → sRGB f32。
//!   出力エンコードと、線形空間から sRGB 空間 op へ入る経路で使う。
//!
//! ## なぜ「4096 エントリ + 線形補間」なのか(精度の見積り)
//!
//! 素引き(最近傍)の 4096 LUT では、暗部で OETF の傾きが急峻
//! (`f'(l) ≈ 12.8` at `l = 0.0031`)なため、格子間隔 `h = 1/4095 = 2.44e-4` に対して
//! 最大誤差は `h/2 * f' ≈ 1.2e-4`(= u8 で 0.031 段、u16 で **8 段**)に達する。
//! u8 出力なら丸めに吸収されるが、16bit 出力では目に見える段差になる。
//!
//! 隣接エントリ間を線形補間すると、誤差は `|f''| * h^2 / 8` に落ちる。
//! OETF の 2 階微分は暗部境界(`l = 0.0031308`)で最大 `|f''| ≈ 2.4e3` なので
//! `2.4e3 * (2.44e-4)^2 / 8 ≈ 1.8e-5`(= u8 で 0.005 段、u16 で **約 1.2 段**)。
//! 素引きより 7 倍良く、u8 では完全に無視でき、u16 でも最暗部で高々 1〜2 LSB に収まる。
//! 逆方向(EOTF)は 2 階微分が高々 `3.0` 程度なので `2.3e-8`(線形光)、
//! 符号値換算で 1e-8 未満と、こちらは桁違いに正確である。
//!
//! この精度は「u8 往復のバイト同一性」に必要な余裕(`0.5/255 = 2.0e-3`)より
//! 2 桁小さいので、`u8 → 線形 → sRGB f32 → (恒等 op) → 線形 → u8` は
//! 常に元のバイト列を返す(tests/engine_v2_quality.rs と hsl 往復テストの硬いゲート)。

use std::sync::OnceLock;

/// sRGB ⇄ 線形の補間 LUT のエントリ数。
const CONV_LUT_LEN: usize = 4096;
/// 補間の格子最大インデックス(f32 定数として持つ)。
const CONV_LUT_LAST: f32 = (CONV_LUT_LEN - 1) as f32;

/// アルファの逆プリマルチプライを諦める閾値。これ未満は RGB を 0 に落とす。
pub(crate) const ALPHA_EPSILON: f32 = 1e-6;

// ---------------------------------------------------------------- 量子化・丸め

/// 値を 1e-9 グリッドへ丸める(libm 差の遮断。f32_spike と同じ規約)。
pub(crate) fn quantize_1e9(v: f64) -> f64 {
    (v * 1e9).round() / 1e9
}

/// 値を 1e-6 グリッドへ丸める(カーネル係数・ゲイン用。ops/mod.rs 規約)。
pub(crate) fn quantize_1e6(v: f64) -> f64 {
    (v * 1e6).round() / 1e6
}

/// half-away-from-zero 丸め(banker's rounding ではない)。
fn round_half_away_from_zero(v: f32) -> f32 {
    if v >= 0.0 {
        (v + 0.5).floor()
    } else {
        (v - 0.5).ceil()
    }
}

/// 0..1 の f32 を u8 へ(クランプ + half-away-from-zero)。
pub(crate) fn unit_to_u8(v: f32) -> u8 {
    let scaled = v.clamp(0.0, 1.0) * 255.0;
    round_half_away_from_zero(scaled).clamp(0.0, 255.0) as u8
}

/// 0..1 の f32 を u16 へ(クランプ + half-away-from-zero)。
pub(crate) fn unit_to_u16(v: f32) -> u16 {
    let scaled = v.clamp(0.0, 1.0) * 65535.0;
    round_half_away_from_zero(scaled).clamp(0.0, 65535.0) as u16
}

// -------------------------------------------------------------- 伝達関数(f64)

/// sRGB EOTF(符号値 → 線形光)。標準の区分関数。
fn srgb_eotf(c: f64) -> f64 {
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// sRGB OETF(線形光 → 符号値)。EOTF の解析的逆関数。
fn srgb_oetf(linear: f64) -> f64 {
    if linear <= 0.003_130_8 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    }
}

// ------------------------------------------------------------------------ LUT

fn eotf_u8_lut() -> &'static [f32; 256] {
    static LUT: OnceLock<[f32; 256]> = OnceLock::new();
    LUT.get_or_init(|| {
        let mut lut = [0f32; 256];
        for (i, slot) in lut.iter_mut().enumerate() {
            *slot = quantize_1e9(srgb_eotf(i as f64 / 255.0)) as f32;
        }
        lut
    })
}

fn eotf_u16_lut() -> &'static [f32] {
    static LUT: OnceLock<Vec<f32>> = OnceLock::new();
    LUT.get_or_init(|| {
        (0..65536)
            .map(|i| quantize_1e9(srgb_eotf(i as f64 / 65535.0)) as f32)
            .collect()
    })
}

fn eotf_f32_lut() -> &'static [f32; CONV_LUT_LEN] {
    static LUT: OnceLock<[f32; CONV_LUT_LEN]> = OnceLock::new();
    LUT.get_or_init(|| {
        let mut lut = [0f32; CONV_LUT_LEN];
        for (i, slot) in lut.iter_mut().enumerate() {
            *slot = quantize_1e9(srgb_eotf(i as f64 / (CONV_LUT_LEN - 1) as f64)) as f32;
        }
        lut
    })
}

fn oetf_f32_lut() -> &'static [f32; CONV_LUT_LEN] {
    static LUT: OnceLock<[f32; CONV_LUT_LEN]> = OnceLock::new();
    LUT.get_or_init(|| {
        let mut lut = [0f32; CONV_LUT_LEN];
        for (i, slot) in lut.iter_mut().enumerate() {
            *slot = quantize_1e9(srgb_oetf(i as f64 / (CONV_LUT_LEN - 1) as f64)) as f32;
        }
        lut
    })
}

/// 4096 エントリ LUT を線形補間で引く(演算順序固定・FMA 不使用)。
#[inline]
fn sample_conv_lut(lut: &[f32; CONV_LUT_LEN], v: f32) -> f32 {
    let clamped = v.clamp(0.0, 1.0);
    let pos = clamped * CONV_LUT_LAST;
    let floor = pos.floor();
    let i0 = floor as usize;
    if i0 >= CONV_LUT_LEN - 1 {
        return lut[CONV_LUT_LEN - 1];
    }
    let frac = pos - floor;
    let a = lut[i0];
    let b = lut[i0 + 1];
    // 乗算と加算を分離して書く(mul_add 禁止)。
    let delta = b - a;
    let step = delta * frac;
    a + step
}

/// sRGB 符号値 f32(0..1)→ 線形光 f32。
#[inline]
pub(crate) fn srgb_to_linear(v: f32) -> f32 {
    sample_conv_lut(eotf_f32_lut(), v)
}

/// 線形光 f32 → sRGB 符号値 f32(0..1)。
#[inline]
pub(crate) fn linear_to_srgb(v: f32) -> f32 {
    sample_conv_lut(oetf_f32_lut(), v)
}

/// u8 sRGB → 線形光(256 エントリの厳密テーブル)。
#[inline]
pub(crate) fn u8_to_linear(v: u8) -> f32 {
    eotf_u8_lut()[v as usize]
}

// ------------------------------------------------------------------- 作業空間

/// 画素バッファの現在の解釈。`LinearImage` のバッファそのものは同じ形で、
/// 「数値が線形光か sRGB 符号値か」だけが違う(§ops/mod.rs の作業空間表)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Space {
    /// RGB は線形光 0..1。幾何・フィルタ系 op の作業空間。
    Linear,
    /// RGB は sRGB 符号値 0..1。トーン・カラー系 op の作業空間。
    Srgb,
}

// --------------------------------------------------------------- LinearImage

/// f32 RGBA バッファ。RGB は `Space` に従う 0..1、**アルファは常に線形 0..1**
/// (アルファに伝達関数は掛けない = ストレートアルファ)。
///
/// 画素はストレート(非プリマルチプライ)で保持する。プリマルチプライが必要な
/// フィルタ系 op は `premultiplied()` / `unpremultiply()` で局所的に往復する。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LinearImage {
    pub width: u32,
    pub height: u32,
    /// 行優先 `width * height` 要素。
    pub data: Vec<[f32; 4]>,
}

impl LinearImage {
    pub(crate) fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            data: vec![[0.0, 0.0, 0.0, 1.0]; width as usize * height as usize],
        }
    }

    pub(crate) fn from_pixel(width: u32, height: u32, px: [f32; 4]) -> Self {
        Self {
            width,
            height,
            data: vec![px; width as usize * height as usize],
        }
    }

    pub(crate) fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    #[inline]
    pub(crate) fn index(&self, x: u32, y: u32) -> usize {
        y as usize * self.width as usize + x as usize
    }

    #[inline]
    pub(crate) fn get(&self, x: u32, y: u32) -> [f32; 4] {
        self.data[self.index(x, y)]
    }

    #[inline]
    pub(crate) fn set(&mut self, x: u32, y: u32, px: [f32; 4]) {
        let i = self.index(x, y);
        self.data[i] = px;
    }

    /// 端をクランプ(複製)して読む。
    #[inline]
    pub(crate) fn get_clamped(&self, x: i64, y: i64) -> [f32; 4] {
        let cx = x.clamp(0, self.width as i64 - 1) as u32;
        let cy = y.clamp(0, self.height as i64 - 1) as u32;
        self.get(cx, cy)
    }

    /// RGB にアルファを掛けたコピーを返す(プリマルチプライ)。
    ///
    /// アルファが全画素 1.0 の画像では 1.0 倍 = 厳密な恒等なので、
    /// 「アルファなし画像の高速パス」は別コードを持たなくてもビット同一になる。
    pub(crate) fn premultiplied(&self) -> LinearImage {
        let mut out = self.clone();
        for px in out.data.iter_mut() {
            let a = px[3];
            px[0] *= a;
            px[1] *= a;
            px[2] *= a;
        }
        out
    }

    /// プリマルチプライを解く。`a < ALPHA_EPSILON` の画素は RGB を 0 にする
    /// (0 除算とノイズ増幅を避けるための固定規則)。RGB/A ともに 0..1 へクランプする。
    pub(crate) fn unpremultiply(&mut self) {
        for px in self.data.iter_mut() {
            let a = px[3].clamp(0.0, 1.0);
            px[3] = a;
            if a < ALPHA_EPSILON {
                px[0] = 0.0;
                px[1] = 0.0;
                px[2] = 0.0;
            } else {
                px[0] = (px[0] / a).clamp(0.0, 1.0);
                px[1] = (px[1] / a).clamp(0.0, 1.0);
                px[2] = (px[2] / a).clamp(0.0, 1.0);
            }
        }
    }

    // ---------------------------------------------------------- デコード/エンコード

    /// RGBA8 → **sRGB 符号値** f32(`v = i / 255`、厳密)。
    ///
    /// 最初に来る空間依存 op が sRGB 空間だった場合、エンジンはこちらでデコードする。
    /// 伝達関数を一切通さないので u8 の符号値がビット精度で保たれ、
    /// 「トーン系 op だけのレシピ」は変換誤差ゼロで走る。
    pub(crate) fn from_rgba8_srgb(img: &image::RgbaImage) -> Self {
        let (width, height) = img.dimensions();
        let data = img
            .pixels()
            .map(|p| {
                [
                    p.0[0] as f32 / 255.0,
                    p.0[1] as f32 / 255.0,
                    p.0[2] as f32 / 255.0,
                    p.0[3] as f32 / 255.0,
                ]
            })
            .collect();
        Self {
            width,
            height,
            data,
        }
    }

    /// RGBA16 → **sRGB 符号値** f32(`v = i / 65535`、厳密)。
    pub(crate) fn from_rgba16_srgb(img: &image::ImageBuffer<image::Rgba<u16>, Vec<u16>>) -> Self {
        let (width, height) = img.dimensions();
        let data = img
            .pixels()
            .map(|p| {
                [
                    p.0[0] as f32 / 65535.0,
                    p.0[1] as f32 / 65535.0,
                    p.0[2] as f32 / 65535.0,
                    p.0[3] as f32 / 65535.0,
                ]
            })
            .collect();
        Self {
            width,
            height,
            data,
        }
    }

    /// RGBA8(sRGB)→ 線形 f32(256 エントリの厳密 EOTF テーブル)。
    pub(crate) fn from_rgba8(img: &image::RgbaImage) -> Self {
        let lut = eotf_u8_lut();
        let (width, height) = img.dimensions();
        let data = img
            .pixels()
            .map(|p| {
                [
                    lut[p.0[0] as usize],
                    lut[p.0[1] as usize],
                    lut[p.0[2] as usize],
                    // アルファは伝達関数の対象外。0..1 へスケールするだけ。
                    p.0[3] as f32 / 255.0,
                ]
            })
            .collect();
        Self {
            width,
            height,
            data,
        }
    }

    /// RGBA16(sRGB)→ 線形 f32(PNG16 入力経路)。
    pub(crate) fn from_rgba16(img: &image::ImageBuffer<image::Rgba<u16>, Vec<u16>>) -> Self {
        let lut = eotf_u16_lut();
        let (width, height) = img.dimensions();
        let data = img
            .pixels()
            .map(|p| {
                [
                    lut[p.0[0] as usize],
                    lut[p.0[1] as usize],
                    lut[p.0[2] as usize],
                    p.0[3] as f32 / 65535.0,
                ]
            })
            .collect();
        Self {
            width,
            height,
            data,
        }
    }

    /// **すでに sRGB 符号値になっているバッファ**を RGBA8 へ量子化する。
    ///
    /// エンジンは出口の直前に必ず `Space::Srgb` へ移す(`engine::ensure_space`)。
    /// こうすると、最後の op が sRGB 空間の op(curves 等)だった場合に
    /// 「線形へ戻して再度符号化する」という無意味な往復が消え、
    /// 変換の補間誤差(符号値で 1.8e-5)も丸め直前に乗らなくなる。
    /// 結果として `levels` のようにノード値がちょうど `.5` に乗る指定でも、
    /// half-away-from-zero が v1 と同じ向きに決まる。
    pub(crate) fn encoded_to_rgba8(&self) -> image::RgbaImage {
        let mut raw = Vec::with_capacity(self.data.len() * 4);
        for px in &self.data {
            raw.push(unit_to_u8(px[0]));
            raw.push(unit_to_u8(px[1]));
            raw.push(unit_to_u8(px[2]));
            raw.push(unit_to_u8(px[3]));
        }
        image::RgbaImage::from_raw(self.width, self.height, raw)
            .expect("buffer sized w*h*4 matches RgbaImage layout")
    }

    /// 同上の 16bit 版(PNG16 出力用)。
    pub(crate) fn encoded_to_rgba16(&self) -> image::ImageBuffer<image::Rgba<u16>, Vec<u16>> {
        let mut raw = Vec::with_capacity(self.data.len() * 4);
        for px in &self.data {
            raw.push(unit_to_u16(px[0]));
            raw.push(unit_to_u16(px[1]));
            raw.push(unit_to_u16(px[2]));
            raw.push(unit_to_u16(px[3]));
        }
        image::ImageBuffer::from_raw(self.width, self.height, raw)
            .expect("buffer sized w*h*4 matches Rgba16 layout")
    }

    // ------------------------------------------------------------- 空間変換

    /// RGB を線形 → sRGB 符号値へ(アルファは不変)。
    pub(crate) fn encode_in_place(&mut self) {
        crate::parallel::for_each_chunk(&mut self.data, |chunk| {
            for px in chunk.iter_mut() {
                px[0] = linear_to_srgb(px[0]);
                px[1] = linear_to_srgb(px[1]);
                px[2] = linear_to_srgb(px[2]);
            }
        });
    }

    /// RGB を sRGB 符号値 → 線形へ(アルファは不変)。
    pub(crate) fn decode_in_place(&mut self) {
        crate::parallel::for_each_chunk(&mut self.data, |chunk| {
            for px in chunk.iter_mut() {
                px[0] = srgb_to_linear(px[0]);
                px[1] = srgb_to_linear(px[1]);
                px[2] = srgb_to_linear(px[2]);
            }
        });
    }
}

/// u8 の RGBA 色(pad 色等)を線形 f32 画素へ。
pub(crate) fn pad_to_linear(pad: [u8; 4]) -> [f32; 4] {
    [
        u8_to_linear(pad[0]),
        u8_to_linear(pad[1]),
        u8_to_linear(pad[2]),
        pad[3] as f32 / 255.0,
    ]
}

/// u8 の RGBA 色を sRGB 符号値 f32 画素へ。
pub(crate) fn pad_to_srgb(pad: [u8; 4]) -> [f32; 4] {
    [
        pad[0] as f32 / 255.0,
        pad[1] as f32 / 255.0,
        pad[2] as f32 / 255.0,
        pad[3] as f32 / 255.0,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **硬いゲート**: `u8 → 線形 → sRGB f32 → 線形 → u8` が全 256 値でバイト同一。
    /// sRGB 空間 op(curves/hsl/levels 等)が恒等のとき出力が 1 バイトも動かないことの根拠。
    #[test]
    fn u8_srgb_linear_roundtrip_is_byte_exact() {
        for i in 0..=255u8 {
            let linear = u8_to_linear(i);
            let encoded = linear_to_srgb(linear);
            let back = srgb_to_linear(encoded);
            assert_eq!(
                unit_to_u8(linear_to_srgb(back)),
                i,
                "roundtrip failed at {i}"
            );
            assert_eq!(unit_to_u8(encoded), i, "single-pass encode failed at {i}");
        }
    }

    /// 補間 LUT の精度がドキュメント記載の見積り(符号値で 5e-5 未満)に収まること。
    /// 素引きなら 1.2e-4 に達する暗部を密にサンプルして確認する。
    #[test]
    fn conv_lut_interpolation_error_is_small() {
        let mut worst = 0.0f64;
        for i in 0..=20_000u32 {
            let l = i as f64 / 20_000.0 * 0.02; // 暗部(誤差が最大になる領域)
            let approx = linear_to_srgb(l as f32) as f64;
            let exact = srgb_oetf(l);
            worst = worst.max((approx - exact).abs());
        }
        assert!(
            worst < 5e-5,
            "worst OETF interpolation error {worst} is too large"
        );
    }

    #[test]
    fn premultiply_roundtrip_preserves_opaque_pixels() {
        let mut img = LinearImage::from_pixel(2, 2, [0.25, 0.5, 0.75, 1.0]);
        let pre = img.premultiplied();
        assert_eq!(pre.data[0], [0.25, 0.5, 0.75, 1.0]);
        img = pre;
        img.unpremultiply();
        assert_eq!(img.data[0], [0.25, 0.5, 0.75, 1.0]);
    }

    #[test]
    fn transparent_pixels_lose_rgb_on_unpremultiply() {
        let mut img = LinearImage::from_pixel(1, 1, [0.5, 0.5, 0.5, 0.0]);
        img.unpremultiply();
        assert_eq!(img.data[0], [0.0, 0.0, 0.0, 0.0]);
    }
}
