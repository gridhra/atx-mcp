//! SOURCE 画素座標 → CURRENT パイプライン座標 の 2D アフィン変換追跡。
//!
//! `Crop { coordinate_space: "source" }` を実現するための土台。
//! パイプラインの各幾何 op(orientation 正規化 / rotate / crop / pad / resize)が
//! 座標系をどう動かしたかを 2x3 行列に畳み込んで保持し、
//! 「元画像のこの矩形」を「今の画像のこの矩形」へ写せるようにする。
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
//! (`Affine::rotate_about` の呼び出し側で行う)。

use crate::recipe::Rect;

/// 2D アフィン変換 `(x, y) -> (a*x + b*y + tx, c*x + d*y + ty)`。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Affine {
    pub a: f64,
    pub b: f64,
    pub tx: f64,
    pub c: f64,
    pub d: f64,
    pub ty: f64,
}

impl Affine {
    pub(crate) const IDENTITY: Affine = Affine {
        a: 1.0,
        b: 0.0,
        tx: 0.0,
        c: 0.0,
        d: 1.0,
        ty: 0.0,
    };

    /// 平行移動。crop は負、pad は正の移動になる。
    pub(crate) fn translate(dx: f64, dy: f64) -> Affine {
        Affine {
            tx: dx,
            ty: dy,
            ..Affine::IDENTITY
        }
    }

    /// 軸ごとの拡大縮小(連続座標なので原点は動かない)。
    pub(crate) fn scale(sx: f64, sy: f64) -> Affine {
        Affine {
            a: sx,
            d: sy,
            ..Affine::IDENTITY
        }
    }

    /// 線形部のみ(EXIF orientation の反転・90 度回転を直接書き下すため)。
    pub(crate) fn linear(a: f64, b: f64, tx: f64, c: f64, d: f64, ty: f64) -> Affine {
        Affine { a, b, tx, c, d, ty }
    }

    /// `from` を中心に `theta` ラジアン回転し、結果を `to` に置く変換。
    ///
    /// `theta` の符号は `imageproc::geometric_transformations::Projection::rotate`
    /// と同じ(画像座標系では正 = 時計回り)。
    pub(crate) fn rotate_about(theta: f64, from: (f64, f64), to: (f64, f64)) -> Affine {
        let (s, c) = theta.sin_cos();
        // p' = R * (p - from) + to
        Affine {
            a: c,
            b: -s,
            tx: to.0 - (c * from.0 - s * from.1),
            c: s,
            d: c,
            ty: to.1 - (s * from.0 + c * from.1),
        }
    }

    /// `self` を適用した後に `next` を適用する合成(= `next ∘ self`)。
    pub(crate) fn then(self, next: Affine) -> Affine {
        Affine {
            a: next.a * self.a + next.b * self.c,
            b: next.a * self.b + next.b * self.d,
            tx: next.a * self.tx + next.b * self.ty + next.tx,
            c: next.c * self.a + next.d * self.c,
            d: next.c * self.b + next.d * self.d,
            ty: next.c * self.tx + next.d * self.ty + next.ty,
        }
    }

    pub(crate) fn map(&self, x: f64, y: f64) -> (f64, f64) {
        (
            self.a * x + self.b * y + self.tx,
            self.c * x + self.d * y + self.ty,
        )
    }

    /// 連続座標の矩形 `[x, x+w] x [y, y+h]` の 4 隅を写し、その軸並行外接矩形
    /// `(min_x, min_y, max_x, max_y)` を返す。
    pub(crate) fn map_box(&self, x: f64, y: f64, w: f64, h: f64) -> (f64, f64, f64, f64) {
        let corners = [
            self.map(x, y),
            self.map(x + w, y),
            self.map(x, y + h),
            self.map(x + w, y + h),
        ];
        let mut min = corners[0];
        let mut max = corners[0];
        for (cx, cy) in corners.iter().skip(1) {
            min.0 = min.0.min(*cx);
            min.1 = min.1.min(*cy);
            max.0 = max.0.max(*cx);
            max.1 = max.1.max(*cy);
        }
        (min.0, min.1, max.0, max.1)
    }
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
/// - 4 隅を写して軸並行外接矩形を取る(回転が挟まっていると、視覚的に傾いた
///   四角形ではなくその外接矩形になる = 元矩形よりわずかに大きい)
/// - 端は half-away-from-zero で丸める
/// - 現在の画像範囲 `[0, cur_w] x [0, cur_h]` と交差を取る
/// - 交差が空なら `Err`(メッセージに写像後の座標を含める)
pub(crate) fn map_source_rect(
    xf: &Affine,
    rect: Rect,
    cur_w: u32,
    cur_h: u32,
) -> std::result::Result<MappedRect, String> {
    let (min_x, min_y, max_x, max_y) = xf.map_box(
        rect.x as f64,
        rect.y as f64,
        rect.width as f64,
        rect.height as f64,
    );
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
