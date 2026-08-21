//! SVG オーバレイ(v0.8)。ベクタアセット(ロゴ・ウォーターマーク・クレジット)を
//! ラスタライズして画像の上に**焼き込む** op。
//!
//! **作業空間: sRGB 符号値**(`ops/mod.rs` の表)。SVG の色指定は CSS の色、つまり
//! sRGB 符号値であり、ラスタライザ(tiny-skia)が返す RGBA8 もその空間の値である。
//! さらに合成そのものがレイヤー合成と同じ W3C compositing-1 の式なので、
//! 「合成は sRGB 符号値空間で行う」という §9.7 の判断をそのまま引き継ぐ
//! (実際、画素合成は [`crate::ops::blend::composite_px`] を**共有**している)。
//!
//! # 決定論とフォント(この op の中核設計)
//!
//! `resvg` は **`default-features = false`** で依存している。既定で有効な
//! `text` / `system-fonts` は **システムにインストールされたフォントを読みに行く**ため、
//! 同じレシピ・同じ SVG が「実行したマシンによって違うバイト列を出す」ことになり、
//! 本プロジェクトの横断規律(バイト同一の再現性)と正面から矛盾する。
//! フォントは OS・バージョン・ユーザのインストール状況で変わり、
//! ヒンティングやフォールバックの選択まで揺れるので、「空の fontdb を渡す」よりも
//! **`text` 機能ごとビルドから外す**方が強い保証になる(fontdb がリンクすらされない)。
//!
//! 帰結として **`<text>` 要素は描画されない**。SVG のソースに `<text` が現れたら
//! 実行時警告を出し、「テキストをパスへ変換せよ(= 決定論的な出力になる)」と伝える。
//! これは制約ではなく契約である: パス化された文字は、どのマシンでも同じ画素になる。
//!
//! ラスタライズ自体(tiny-skia)は f32 スカラ/固定小数の演算で乱数も反復も持たないので
//! 決定論的である。`tests/svg_overlay.rs` の「2 回実行してバイト同一」がこれを固定する。
//!
//! # 寸法ルール
//!
//! | `width` | `height` | ラスタ寸法 |
//! |---|---|---|
//! | なし | なし | SVG の**固有サイズ**(width/height 属性、無ければ viewBox) |
//! | あり | なし | 幅を合わせ、高さは固有サイズの縦横比から導く |
//! | なし | あり | その逆 |
//! | あり | あり | 指定どおり(縦横比は無視) |
//!
//! 固有サイズを持たない SVG(viewBox が無く、width/height も無い or `%` 指定)は
//! usvg が既定値 100x100 で代替してしまうので、**その値に依存した結果を黙って返さない**。
//! `width` と `height` の両方が与えられていない限り構造化エラーにする。

use resvg::usvg;

use crate::linear::LinearImage;
use crate::recipe::BlendMode;
use crate::{AtxError, Result};

/// ラスタ 1 辺の上限(validate)。実務のロゴ・ウォーターマークには十分広い。
const MAX_RASTER_EDGE: u32 = 32_768;
/// ラスタ画素数の上限(実行時)。`Limits::max_pixels` の既定と同じ 100MP。
const MAX_RASTER_PIXELS: u64 = 100_000_000;

/// `<text>` を含む SVG に対する実行時警告(文言はテストで固定する)。
pub(crate) const TEXT_WARNING: &str =
    "svg contains text elements; text is not rendered (convert text to paths for \
     deterministic output)";

/// `svg_overlay` の静的検証(入力バイト列に依存しない制約のみ)。
pub fn validate(
    index: usize,
    svg_revision_id: &str,
    opacity: f64,
    width: Option<u32>,
    height: Option<u32>,
) -> Result<()> {
    if svg_revision_id.is_empty() {
        return Err(AtxError::InvalidRecipe(format!(
            "operations[{index}] (svg_overlay): svg_revision_id must not be empty"
        )));
    }
    if !svg_revision_id.starts_with("rev_") {
        return Err(AtxError::InvalidRecipe(format!(
            "operations[{index}] (svg_overlay): svg_revision_id must start with \"rev_\", \
             got {svg_revision_id:?}"
        )));
    }
    if !opacity.is_finite() || !(0.0..=1.0).contains(&opacity) {
        return Err(AtxError::InvalidRecipe(format!(
            "operations[{index}] (svg_overlay): opacity must be within 0.0..=1.0, got {opacity}"
        )));
    }
    for (name, v) in [("width", width), ("height", height)] {
        if let Some(v) = v {
            if v == 0 {
                return Err(AtxError::InvalidRecipe(format!(
                    "operations[{index}] (svg_overlay): {name} must be > 0 when given"
                )));
            }
            if v > MAX_RASTER_EDGE {
                return Err(AtxError::InvalidRecipe(format!(
                    "operations[{index}] (svg_overlay): {name} must be within \
                     1..={MAX_RASTER_EDGE}, got {v}"
                )));
            }
        }
    }
    Ok(())
}

/// 決定論のためのフォント無し usvg オプション。
///
/// `resvg` を `default-features = false` でビルドしているため
/// `Options` に `fontdb` / `font_resolver` フィールドは**存在しない**
/// (= システムフォントを読む経路がコンパイル時に消えている)。
fn options() -> usvg::Options<'static> {
    usvg::Options::default()
}

/// UTF-8 テキストとして SVG ソースを取り出す(BOM を落とす)。
///
/// svgz(gzip)は `resvg` の `svgz` 機能ごと外しているので受け付けない。
fn source_text(bytes: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(bytes).ok()?;
    Some(text.trim_start_matches('\u{feff}'))
}

/// SVG の**固有サイズ**があるかどうかを、ルート要素の属性から判定する。
///
/// usvg の解決規則(`parser::converter::resolve_svg_size`)の裏返し:
/// `viewBox` があればそれが固有サイズを与え、無い場合は width/height が
/// **両方とも絶対長**でなければ `Options::default_size`(100x100)で代替される。
/// その代替値に依存した結果を黙って返さないために、ここで先に見分ける。
fn has_intrinsic_size(root: &usvg::roxmltree::Node) -> bool {
    if root.has_attribute("viewBox") {
        return true;
    }
    let absolute = |name: &str| {
        root.attribute(name)
            .map(str::trim)
            .is_some_and(|v| !v.is_empty() && !v.ends_with('%'))
    };
    absolute("width") && absolute("height")
}

/// SVG バイト列の固有サイズ(px、half-away-from-zero 丸め)。
///
/// `import_asset` が台帳へ寸法を記録するために使う公開ヘルパ
/// (atx-mcp が resvg へ直接依存しなくて済むよう、core 側に 1 本だけ生やす)。
/// パースできない・固有サイズを持たない SVG では `None`。
pub fn intrinsic_size(bytes: &[u8]) -> Option<(u32, u32)> {
    let text = source_text(bytes)?;
    let doc = usvg::roxmltree::Document::parse(text).ok()?;
    if !has_intrinsic_size(&doc.root_element()) {
        return None;
    }
    let tree = usvg::Tree::from_xmltree(&doc, &options()).ok()?;
    let size = tree.size();
    let w = round_positive(size.width() as f64)?;
    let h = round_positive(size.height() as f64)?;
    Some((w, h))
}

/// 正の有限 f64 を u32 へ half-away-from-zero 丸めする(最低 1)。
fn round_positive(v: f64) -> Option<u32> {
    if !v.is_finite() || v <= 0.0 {
        return None;
    }
    let r = v.round();
    if r > u32::MAX as f64 {
        return None;
    }
    Some((r as u32).max(1))
}

/// ラスタライズ済みのオーバレイ。sRGB 符号値・**ストレートアルファ**。
#[derive(Debug)]
pub(crate) struct Raster {
    pub img: LinearImage,
    pub warnings: Vec<String>,
}

/// SVG を目標寸法へラスタライズする。
///
/// 失敗は呼び出し側(engine)が `AtxError::Operation` へ包む前提の平文メッセージで返す。
pub(crate) fn rasterize(
    bytes: &[u8],
    width: Option<u32>,
    height: Option<u32>,
) -> std::result::Result<Raster, String> {
    let text = source_text(bytes).ok_or_else(|| {
        "the referenced asset is not valid UTF-8 XML; svg_overlay needs a plain (non-gzipped) \
         .svg file"
            .to_string()
    })?;
    let doc = usvg::roxmltree::Document::parse(text)
        .map_err(|e| format!("the referenced asset is not parseable XML: {e}"))?;
    let root = doc.root_element();
    if !root.has_tag_name("svg") {
        return Err(format!(
            "the referenced asset's root element is <{}>, not <svg>",
            root.tag_name().name()
        ));
    }
    let intrinsic = has_intrinsic_size(&root);
    let tree = usvg::Tree::from_xmltree(&doc, &options())
        .map_err(|e| format!("the referenced asset is not a renderable SVG: {e}"))?;

    // 目標寸法。固有サイズが無い SVG は usvg の既定 100x100 に落ちるので、
    // width/height の両方で寸法が確定しているときだけ受け入れる。
    let size = tree.size();
    let (iw, ih) = (size.width() as f64, size.height() as f64);
    let (tw, th) = match (width, height) {
        (Some(w), Some(h)) => (w, h),
        _ if !intrinsic => {
            return Err(
                "this SVG has no intrinsic size (no viewBox, and no absolute width/height on \
                 the root <svg>), so the raster size cannot be derived from it; give both \
                 width and height on the svg_overlay operation, or add a viewBox to the SVG"
                    .to_string(),
            );
        }
        // 片方だけ指定 = 固有サイズの縦横比を保って拡縮する。
        (Some(w), None) => {
            let h = round_positive(w as f64 * ih / iw)
                .ok_or_else(|| format!("cannot derive a height from width {w}"))?;
            (w, h)
        }
        (None, Some(h)) => {
            let w = round_positive(h as f64 * iw / ih)
                .ok_or_else(|| format!("cannot derive a width from height {h}"))?;
            (w, h)
        }
        (None, None) => (
            round_positive(iw).ok_or("the SVG's intrinsic width is not usable")?,
            round_positive(ih).ok_or("the SVG's intrinsic height is not usable")?,
        ),
    };
    if tw as u64 * th as u64 > MAX_RASTER_PIXELS {
        return Err(format!(
            "the requested raster is {tw}x{th} = {} pixels, over the {MAX_RASTER_PIXELS} pixel \
             limit for an svg_overlay raster",
            tw as u64 * th as u64
        ));
    }

    let mut warnings = Vec::new();
    // フォントを一切読まないので `<text>` は描画されない(モジュール冒頭の設計note)。
    // 判定は素朴な文字列走査で十分(誤検出しても警告が 1 本増えるだけ)。
    if text.contains("<text") {
        warnings.push(TEXT_WARNING.to_string());
    }

    let mut pixmap = resvg::tiny_skia::Pixmap::new(tw, th)
        .ok_or_else(|| format!("could not allocate a {tw}x{th} raster for the SVG"))?;
    // 固有サイズ → 目標寸法への一様でないスケール(fill 相当)。
    let transform = resvg::tiny_skia::Transform::from_scale(
        tw as f32 / size.width(),
        th as f32 / size.height(),
    );
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    // tiny-skia の Pixmap は **プリマルチプライ済み RGBA8**。
    // atx の中間表現はストレートアルファなので、ここで解く
    // (`a == 0` は RGB を 0 にする = `linear.rs` の unpremultiply と同じ規則)。
    let mut img = LinearImage::new(tw, th);
    for (dst, px) in img.data.iter_mut().zip(pixmap.pixels().iter()) {
        let a8 = px.alpha();
        if a8 == 0 {
            *dst = [0.0, 0.0, 0.0, 0.0];
            continue;
        }
        let a = a8 as f32 / 255.0;
        let unmul = |c: u8| -> f32 {
            let v = c as f32 / 255.0;
            let v = v / a;
            v.clamp(0.0, 1.0)
        };
        *dst = [unmul(px.red()), unmul(px.green()), unmul(px.blue()), a];
    }

    Ok(Raster { img, warnings })
}

/// ラスタを `(x, y)`(左上・現在の画像座標)へ合成する。
///
/// - 画像の外へ出た部分は**クリップ**する(負の座標も可)
/// - 画素合成はレイヤー合成と**同じ関数** [`crate::ops::blend::composite_px`]
///   (αs = ラスタのアルファ × opacity、マスク重みは 1.0)
/// - `img` は呼び出し側で sRGB 符号値空間にしてあること
pub(crate) fn apply(
    img: &mut LinearImage,
    raster: &LinearImage,
    x: i64,
    y: i64,
    mode: BlendMode,
    opacity: f32,
) {
    let (cw, ch) = img.dimensions();
    let (rw, rh) = raster.dimensions();
    for ry in 0..rh {
        let iy = y + ry as i64;
        if iy < 0 || iy >= ch as i64 {
            continue;
        }
        for rx in 0..rw {
            let ix = x + rx as i64;
            if ix < 0 || ix >= cw as i64 {
                continue;
            }
            let src = raster.data[(ry as usize) * rw as usize + rx as usize];
            let di = (iy as usize) * cw as usize + ix as usize;
            crate::ops::blend::composite_px(&mut img.data[di], &src, mode, opacity, 1.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BADGE: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="4"><rect x="0" y="0" width="8" height="4" fill="#ff0000"/></svg>"##;

    #[test]
    fn intrinsic_size_reads_the_root_attributes() {
        assert_eq!(intrinsic_size(BADGE.as_bytes()), Some((8, 4)));
    }

    /// viewBox だけの SVG も固有サイズを持つ。
    #[test]
    fn viewbox_alone_is_an_intrinsic_size() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 10"><rect width="20" height="10"/></svg>"#;
        assert_eq!(intrinsic_size(svg.as_bytes()), Some((20, 10)));
    }

    /// viewBox が無く width/height が % の SVG は「固有サイズ無し」。
    /// usvg は既定の 100x100 を返すが、それに依存させない。
    #[test]
    fn percent_size_without_viewbox_has_no_intrinsic_size() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="100%" height="100%"><rect width="10" height="10"/></svg>"#;
        assert_eq!(intrinsic_size(svg.as_bytes()), None);
        let err = rasterize(svg.as_bytes(), None, None).unwrap_err();
        assert!(err.contains("no intrinsic size"), "{err}");
        // 両方指定すれば通る。
        assert!(rasterize(svg.as_bytes(), Some(4), Some(4)).is_ok());
    }

    #[test]
    fn not_svg_and_not_xml_are_distinct_errors() {
        assert!(intrinsic_size(b"\x89PNG\r\n").is_none());
        let err = rasterize(b"not xml at all", None, None).unwrap_err();
        assert!(err.contains("not parseable XML"), "{err}");
        let err = rasterize(b"<html><body/></html>", None, None).unwrap_err();
        assert!(err.contains("not <svg>"), "{err}");
    }

    /// 幅だけ指定すると縦横比が保たれる(8x4 → 幅 16 なら高さ 8)。
    #[test]
    fn width_only_preserves_the_aspect_ratio() {
        let r = rasterize(BADGE.as_bytes(), Some(16), None).unwrap();
        assert_eq!(r.img.dimensions(), (16, 8));
        let r = rasterize(BADGE.as_bytes(), None, Some(8)).unwrap();
        assert_eq!(r.img.dimensions(), (16, 8));
    }

    /// 不透明な赤い矩形はストレートアルファで厳密に (1, 0, 0, 1) になる。
    #[test]
    fn opaque_fill_unpremultiplies_exactly() {
        let r = rasterize(BADGE.as_bytes(), None, None).unwrap();
        assert_eq!(r.img.dimensions(), (8, 4));
        assert_eq!(r.img.get(4, 2), [1.0, 0.0, 0.0, 1.0]);
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn text_elements_raise_a_warning() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="4"><text x="0" y="3">hi</text></svg>"#;
        let r = rasterize(svg.as_bytes(), None, None).unwrap();
        assert_eq!(r.warnings, vec![TEXT_WARNING.to_string()]);
    }

    #[test]
    fn validate_matrix() {
        assert!(validate(0, "rev_x", 1.0, None, None).is_ok());
        assert!(validate(0, "", 1.0, None, None).is_err());
        assert!(validate(0, "nope", 1.0, None, None).is_err());
        assert!(validate(0, "rev_x", 1.5, None, None).is_err());
        assert!(validate(0, "rev_x", f64::NAN, None, None).is_err());
        assert!(validate(0, "rev_x", 1.0, Some(0), None).is_err());
        assert!(validate(0, "rev_x", 1.0, None, Some(0)).is_err());
        assert!(validate(0, "rev_x", 1.0, Some(MAX_RASTER_EDGE + 1), None).is_err());
    }

    /// 負の座標でも panic せず、はみ出し部分だけクリップされる。
    #[test]
    fn negative_placement_clips() {
        let mut img = LinearImage::from_pixel(4, 4, [0.0, 0.0, 0.0, 1.0]);
        let raster = LinearImage::from_pixel(4, 4, [1.0, 1.0, 1.0, 1.0]);
        apply(&mut img, &raster, -2, -2, BlendMode::Normal, 1.0);
        assert_eq!(img.get(0, 0), [1.0, 1.0, 1.0, 1.0]);
        assert_eq!(img.get(1, 1), [1.0, 1.0, 1.0, 1.0]);
        assert_eq!(img.get(2, 2), [0.0, 0.0, 0.0, 1.0]);
    }
}
