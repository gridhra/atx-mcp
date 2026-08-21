//! SOURCE 画素座標 → CURRENT パイプライン座標 の 2D 射影変換追跡。
//!
//! `Crop { coordinate_space: "source" }` を実現するための土台。
//! パイプラインの各幾何 op(orientation 正規化 / rotate / crop / pad / resize /
//! perspective)が座標系をどう動かしたかを **3x3 同次行列**に畳み込んで保持し、
//! 「元画像のこの矩形」を「今の画像のこの矩形」へ写せるようにする。
//!
//! v0.2 で 2x3 アフィンから 3x3 射影へ拡張した(`perspective` op のため)。
//! アフィンは射影の部分集合(最下行 `[0, 0, 1]`)なので、既存の幾何 op の追跡は
//! そのまま埋め込まれ、出力バイト列も座標写像の結果も変わらない。
//! 既存の呼び出し側のために `Affine` は `Transform` の型エイリアスとして残してある
//! (実体が射影行列でもアフィン op が作るのは常にアフィン行列である、という読み方)。
//!
//! # 座標系の約束
//!
//! 行列は **連続座標**(画素 index `i` は区間 `[i, i+1)` を占める。つまり画素中心は
//! `i + 0.5`)で定義する。整数 index ではなく連続座標を使う理由は、リサイズ
//! (`u' = s * u`)と反転(`u' = w - u`)が連続座標でのみ厳密に線形になり、
//! 矩形の端の扱いが半画素ずれないため。
//!
//! `imageproc` の `warp` 系は index 座標で回転中心を `(w/2, h/2)` と置くので、
//! 任意角回転だけは連続座標へ換算した中心 `(w/2 + 0.5, h/2 + 0.5)` を使う
//! (`Affine::rotate_about` の呼び出し側で行う)。射影 warp でも同様に、
//! 連続座標の行列を index 座標へ `T(-0.5) ∘ H ∘ T(+0.5)` で換算してから渡す
//! (`ops::perspective` 側で行う)。

use crate::recipe::Rect;

/// 2D 射影変換(同次座標の 3x3 行列、行優先)。
///
/// `(x, y) -> ((m00 x + m01 y + m02) / w, (m10 x + m11 y + m12) / w)`、
/// `w = m20 x + m21 y + m22`。アフィンは `m20 = m21 = 0, m22 = 1` の場合。
///
/// 行列は「`m22 > 0` になるようスケール正規化されている」ことを前提に扱う
/// (同次行列は定数倍の自由度があるが、`w > 0` を「像が地平線の手前にある」
/// 判定に使いたいため符号を固定する)。本モジュールのコンストラクタは
/// すべてこの正規形を作る。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Transform {
    pub m: [[f64; 3]; 3],
}

/// アフィン専用だった頃の名前。既存の幾何 op(orientation / rotate / crop /
/// pad / resize)はアフィン行列しか作らないので、呼び出し側はこの別名を使い続ける。
pub(crate) type Affine = Transform;

impl Transform {
    pub(crate) const IDENTITY: Transform = Transform {
        m: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
    };

    /// 平行移動。crop は負、pad は正の移動になる。
    pub(crate) fn translate(dx: f64, dy: f64) -> Transform {
        Transform {
            m: [[1.0, 0.0, dx], [0.0, 1.0, dy], [0.0, 0.0, 1.0]],
        }
    }

    /// 軸ごとの拡大縮小(連続座標なので原点は動かない)。
    pub(crate) fn scale(sx: f64, sy: f64) -> Transform {
        Transform {
            m: [[sx, 0.0, 0.0], [0.0, sy, 0.0], [0.0, 0.0, 1.0]],
        }
    }

    /// アフィン線形部 `(a x + b y + tx, c x + d y + ty)`
    /// (EXIF orientation の反転・90 度回転を直接書き下すため)。
    pub(crate) fn linear(a: f64, b: f64, tx: f64, c: f64, d: f64, ty: f64) -> Transform {
        Transform {
            m: [[a, b, tx], [c, d, ty], [0.0, 0.0, 1.0]],
        }
    }

    /// 3x3 同次行列(行優先)から作る。`m22` で割って正規形へ揃える。
    ///
    /// `m22` が 0 / 非有限のときは正規化できないので、行列をそのまま保持する
    /// (この形が実際に使われるのは、画像平面が地平線を跨ぐ極端な射影だけで、
    /// `map_box` の `w > 0` 検査で弾かれる)。
    pub(crate) fn projective(m: [[f64; 3]; 3]) -> Transform {
        let d = m[2][2];
        if d.is_finite() && d != 0.0 && d != 1.0 {
            let mut n = m;
            for row in &mut n {
                for v in row {
                    *v /= d;
                }
            }
            return Transform { m: n };
        }
        Transform { m }
    }

    /// `from` を中心に `theta` ラジアン回転し、結果を `to` に置く変換。
    ///
    /// `theta` の符号は `imageproc::geometric_transformations::Projection::rotate`
    /// と同じ(画像座標系では正 = 時計回り)。
    pub(crate) fn rotate_about(theta: f64, from: (f64, f64), to: (f64, f64)) -> Transform {
        let (s, c) = theta.sin_cos();
        // p' = R * (p - from) + to
        Transform::linear(
            c,
            -s,
            to.0 - (c * from.0 - s * from.1),
            s,
            c,
            to.1 - (s * from.0 + c * from.1),
        )
    }

    /// `self` を適用した後に `next` を適用する合成(= 行列積 `next * self`)。
    pub(crate) fn then(self, next: Transform) -> Transform {
        let mut m = [[0.0f64; 3]; 3];
        for (i, row) in m.iter_mut().enumerate() {
            for (j, v) in row.iter_mut().enumerate() {
                *v = next.m[i][0] * self.m[0][j]
                    + next.m[i][1] * self.m[1][j]
                    + next.m[i][2] * self.m[2][j];
            }
        }
        Transform { m }
    }

    /// 同次座標での像 `(x', y', w')`(透視除算前)。
    pub(crate) fn map_homogeneous(&self, x: f64, y: f64) -> (f64, f64, f64) {
        (
            self.m[0][0] * x + self.m[0][1] * y + self.m[0][2],
            self.m[1][0] * x + self.m[1][1] * y + self.m[1][2],
            self.m[2][0] * x + self.m[2][1] * y + self.m[2][2],
        )
    }

    /// 点を写す(透視除算込み)。アフィンでは `w' = 1.0` ちょうどなので、
    /// 除算は IEEE 的に恒等であり、従来のアフィン実装と 1 ULP も違わない。
    pub(crate) fn map(&self, x: f64, y: f64) -> (f64, f64) {
        let (hx, hy, w) = self.map_homogeneous(x, y);
        (hx / w, hy / w)
    }

    /// 逆変換(余因子行列)。特異なときは `None`。
    pub(crate) fn inverse(&self) -> Option<Transform> {
        let m = &self.m;
        let c = [
            [
                m[1][1] * m[2][2] - m[1][2] * m[2][1],
                m[0][2] * m[2][1] - m[0][1] * m[2][2],
                m[0][1] * m[1][2] - m[0][2] * m[1][1],
            ],
            [
                m[1][2] * m[2][0] - m[1][0] * m[2][2],
                m[0][0] * m[2][2] - m[0][2] * m[2][0],
                m[0][2] * m[1][0] - m[0][0] * m[1][2],
            ],
            [
                m[1][0] * m[2][1] - m[1][1] * m[2][0],
                m[0][1] * m[2][0] - m[0][0] * m[2][1],
                m[0][0] * m[1][1] - m[0][1] * m[1][0],
            ],
        ];
        let det = m[0][0] * c[0][0] + m[0][1] * c[1][0] + m[0][2] * c[2][0];
        if !det.is_finite() || det == 0.0 {
            return None;
        }
        let mut n = c;
        for row in &mut n {
            for v in row {
                *v /= det;
            }
        }
        Some(Transform::projective(n))
    }

    /// アフィン(= 最下行が `[0, 0, 1]`)か。
    #[cfg(test)]
    pub(crate) fn is_affine(&self) -> bool {
        self.m[2][0] == 0.0 && self.m[2][1] == 0.0 && self.m[2][2] == 1.0
    }

    /// 3x3 の全要素を 1e-6 グリッドへ量子化する(`m22` 正規化つき)。
    ///
    /// `ops` の決定論規約(`ops/mod.rs`)に従い、libm 由来の値から組んだ行列は
    /// **適用前に必ずこのグリッドへ落とす**。ただし 1e-6 という刻みが意味を持つのは
    /// 係数が O(1) の座標系に限るので、呼び出し側は「画像を単位長に正規化した座標系」
    /// で行列を組んでから量子化し、そのあとで画素座標へスケールし直すこと
    /// (`ops::perspective` を参照)。
    pub(crate) fn quantized(self) -> Transform {
        let n = Transform::projective(self.m);
        let mut m = n.m;
        for row in &mut m {
            for v in row {
                *v = quantize_1e6(*v);
            }
        }
        Transform { m }
    }

    /// 連続座標の矩形 `[x, x+w] x [y, y+h]` の 4 隅を写し、その軸並行外接矩形
    /// `(min_x, min_y, max_x, max_y)` を返す。
    ///
    /// 射影変換では 4 隅の外接矩形が矩形全体の像を必ず含む保証は一般には無い
    /// (地平線 `w' = 0` を跨ぐ場合に像が二葉に割れる)。そのため全隅で `w' > 0`
    /// を要求し、満たさなければ `None` を返す。`perspective` op は
    /// `|angle| <= 45°`(および凸四角形)に制限されているので、実際の画像内の矩形が
    /// この検査に落ちることはない。
    pub(crate) fn map_box(&self, x: f64, y: f64, w: f64, h: f64) -> Option<(f64, f64, f64, f64)> {
        let mut min = (f64::INFINITY, f64::INFINITY);
        let mut max = (f64::NEG_INFINITY, f64::NEG_INFINITY);
        for (cx, cy) in [(x, y), (x + w, y), (x, y + h), (x + w, y + h)] {
            let (hx, hy, hw) = self.map_homogeneous(cx, cy);
            if !hw.is_finite() || hw <= 0.0 {
                return None;
            }
            let (px, py) = (hx / hw, hy / hw);
            min.0 = min.0.min(px);
            min.1 = min.1.min(py);
            max.0 = max.0.max(px);
            max.1 = max.1.max(py);
        }
        Some((min.0, min.1, max.0, max.1))
    }
}

/// 1e-6 グリッドへの量子化(`recipe` の canonical 量子化と同じ規則)。
/// 丸めは half-away-from-zero(`f64::round`)。
pub(crate) fn quantize_1e6(v: f64) -> f64 {
    const MAX_EXACT: f64 = 9_007_199_254_740_992.0; // 2^53
    if !v.is_finite() {
        return v;
    }
    let scaled = v * 1e6;
    if !scaled.is_finite() || scaled.abs() >= MAX_EXACT {
        return v;
    }
    scaled.round() / 1e6
}

/// 座標マッピングの結果。
#[derive(Debug, Clone, Copy)]
pub(crate) struct MappedRect {
    pub rect: Rect,
    /// 現在の画像範囲でクランプされた(= 元矩形の一部が画像外だった)か。
    pub clamped: bool,
    /// クランプ前の写像結果(警告・エラーメッセージ用、丸め後)。
    pub raw: (i64, i64, i64, i64),
}

/// 半数は 0 から遠い側へ丸める(`f64::round` の定義そのもの)。
/// 決定論のため丸めは必ずこの関数を通す。
fn round_half_away_from_zero(v: f64) -> f64 {
    v.round()
}

/// SOURCE 座標の矩形を CURRENT 座標へ写し、現在の画像範囲へクランプする。
///
/// - 4 隅を写して軸並行外接矩形を取る(回転や射影が挟まっていると、視覚的に傾いた
///   四角形ではなくその外接矩形になる = 元矩形よりわずかに大きい)
/// - 端は half-away-from-zero で丸める
/// - 現在の画像範囲 `[0, cur_w] x [0, cur_h]` と交差を取る
/// - 交差が空なら `Err`(メッセージに写像後の座標を含める)
pub(crate) fn map_source_rect(
    xf: &Transform,
    rect: Rect,
    cur_w: u32,
    cur_h: u32,
) -> std::result::Result<MappedRect, String> {
    let mapped = xf.map_box(
        rect.x as f64,
        rect.y as f64,
        rect.width as f64,
        rect.height as f64,
    );
    let Some((min_x, min_y, max_x, max_y)) = mapped else {
        return Err(format!(
            "source-space crop rect {}x{}+{}+{} maps across the horizon of the pipeline's \
             projective transform (a corner lands behind the camera); \
             use coordinate_space \"current\" or a milder perspective correction",
            rect.width, rect.height, rect.x, rect.y
        ));
    };
    if ![min_x, min_y, max_x, max_y].iter().all(|v| v.is_finite()) {
        return Err(format!(
            "source-space crop rect {}x{}+{}+{} mapped to a non-finite region \
             (the pipeline transform is degenerate)",
            rect.width, rect.height, rect.x, rect.y
        ));
    }

    // 一旦 i64 に落としてからクランプする(u32 の範囲外へ出る写像もありうる)。
    let clamp_i64 = |v: f64| -> i64 {
        round_half_away_from_zero(v).clamp(i64::MIN as f64, i64::MAX as f64) as i64
    };
    let raw = (
        clamp_i64(min_x),
        clamp_i64(min_y),
        clamp_i64(max_x),
        clamp_i64(max_y),
    );

    let x0 = raw.0.clamp(0, cur_w as i64);
    let y0 = raw.1.clamp(0, cur_h as i64);
    let x1 = raw.2.clamp(0, cur_w as i64);
    let y1 = raw.3.clamp(0, cur_h as i64);

    if x1 <= x0 || y1 <= y0 {
        return Err(format!(
            "source-space crop rect {}x{}+{}+{} maps to [{}, {}]x[{}, {}] in the current \
             {cur_w}x{cur_h} image, which does not intersect it; \
             use coordinate_space \"current\" or widen the rect",
            rect.width, rect.height, rect.x, rect.y, raw.0, raw.2, raw.1, raw.3
        ));
    }

    let clamped = (raw.0, raw.1, raw.2, raw.3) != (x0, y0, x1, y1);
    Ok(MappedRect {
        rect: Rect {
            x: x0 as u32,
            y: y0 as u32,
            width: (x1 - x0) as u32,
            height: (y1 - y0) as u32,
        },
        clamped,
        raw,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_maps_rect_unchanged() {
        let r = Rect {
            x: 5,
            y: 7,
            width: 20,
            height: 10,
        };
        let m = map_source_rect(&Affine::IDENTITY, r, 100, 100).unwrap();
        assert_eq!(m.rect, r);
        assert!(!m.clamped);
    }

    #[test]
    fn compose_is_apply_self_then_next() {
        let a = Affine::translate(10.0, 0.0);
        let b = Affine::scale(2.0, 2.0);
        // まず +10 してから 2 倍 → (0,0) は (20, 0)
        assert_eq!(a.then(b).map(0.0, 0.0), (20.0, 0.0));
        // 2 倍してから +10 → (0,0) は (10, 0)
        assert_eq!(b.then(a).map(0.0, 0.0), (10.0, 0.0));
    }

    #[test]
    fn quarter_turn_matches_rotate90_semantics() {
        // 40x20 を 90 度時計回り → 出力 20x40、(0,0) は右上へ。
        let (w, h) = (40.0, 20.0);
        let xf = Affine::rotate_about(
            std::f64::consts::FRAC_PI_2,
            (w / 2.0, h / 2.0),
            (h / 2.0, w / 2.0),
        );
        let (x, y) = xf.map(0.0, 0.0);
        assert!((x - 20.0).abs() < 1e-9, "{x}");
        assert!(y.abs() < 1e-9, "{y}");
    }

    #[test]
    fn empty_intersection_is_an_error() {
        let xf = Affine::translate(-500.0, 0.0);
        let err = map_source_rect(
            &xf,
            Rect {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
            },
            100,
            100,
        )
        .unwrap_err();
        assert!(err.contains("does not intersect"), "{err}");
    }

    /// 射影行列: 透視除算が効いていること + アフィン判定。
    #[test]
    fn projective_applies_perspective_divide() {
        // w' = 1 + x/100。x = 100 では像が半分に縮む。
        let p = Transform::projective([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.01, 0.0, 1.0]]);
        assert!(!p.is_affine());
        assert_eq!(p.map(0.0, 50.0), (0.0, 50.0));
        assert_eq!(p.map(100.0, 100.0), (50.0, 50.0));
        assert!(Transform::IDENTITY.is_affine());
    }

    #[test]
    fn inverse_round_trips() {
        let p = Transform::projective([[1.2, 0.1, -3.0], [0.0, 0.9, 7.0], [0.002, -0.001, 1.0]]);
        let inv = p.inverse().expect("non-singular");
        for (x, y) in [(0.0, 0.0), (120.0, 30.0), (-40.0, 90.0)] {
            let (fx, fy) = p.map(x, y);
            let (bx, by) = inv.map(fx, fy);
            assert!((bx - x).abs() < 1e-9 && (by - y).abs() < 1e-9, "{bx} {by}");
        }
    }

    /// 合成は「アフィン → 射影」の順でも射影行列として畳み込まれる。
    #[test]
    fn affine_then_projective_composes() {
        let a = Transform::translate(10.0, 0.0);
        let p = Transform::projective([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.01, 0.0, 1.0]]);
        let c = a.then(p);
        assert!(!c.is_affine());
        // (90, 0) → +10 で (100, 0) → 透視除算で (50, 0)
        assert_eq!(c.map(90.0, 0.0), (50.0, 0.0));
    }

    /// 地平線を跨ぐ矩形は写像不能として弾く(外接矩形が像を含まないため)。
    #[test]
    fn rect_across_the_horizon_is_an_error() {
        // w' = 1 - x/50 なので x = 50 で地平線。
        let p = Transform::projective([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [-0.02, 0.0, 1.0]]);
        let err = map_source_rect(
            &p,
            Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 10,
            },
            100,
            100,
        )
        .unwrap_err();
        assert!(err.contains("horizon"), "{err}");
    }

    /// 1e-6 グリッド量子化。
    #[test]
    fn quantization_snaps_to_the_grid() {
        assert_eq!(quantize_1e6(0.123_456_789), 0.123_457);
        assert_eq!(quantize_1e6(-0.000_000_4), -0.0);
        assert!(quantize_1e6(f64::INFINITY).is_infinite());
    }

    #[test]
    fn partial_overlap_is_clamped() {
        let xf = Affine::translate(-5.0, 0.0);
        let m = map_source_rect(
            &xf,
            Rect {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
            },
            100,
            100,
        )
        .unwrap();
        assert!(m.clamped);
        assert_eq!(
            m.rect,
            Rect {
                x: 0,
                y: 0,
                width: 5,
                height: 10
            }
        );
    }
}
