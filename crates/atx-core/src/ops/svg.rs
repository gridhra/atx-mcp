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
//! `resvg` は `default-features = false` + `features = ["text"]` で依存している。
//! 既定で有効な **`system-fonts` / `memmap-fonts` は付けない**: これらは
//! システムにインストールされたフォントを読みに行くため、同じレシピ・同じ SVG が
//! 「実行したマシンによって違うバイト列を出す」ことになり、本プロジェクトの横断規律
//! (バイト同一の再現性)と正面から矛盾する。`text` 単独では usvg のフォント DB
//! (`fontdb`)は**空**で、こちらが明示的に `load_font_data` したバイト列だけが使われる。
//! 整形器は harfrust + skrifa(純 Rust、乱数も環境参照も無い)。
//!
//! この上で、文字描画は **オプトイン**(`svg_overlay.render_text: true`)である:
//!
//! - `render_text: false`(既定): fontdb は空のまま。どの `font-family` も解決できず
//!   **`<text>` 要素は描画されない**。SVG のソースに `<text` が現れたら実行時警告
//!   [`TEXT_WARNING`] を出し、「テキストをパスへ変換せよ」と伝える。v0.8 からの挙動で、
//!   既存レシピの出力バイト列は 1 ビットも動かない。
//! - `render_text: true`: fontdb に**同梱書体**(`assets/fonts/Roboto-Regular.ttf`、
//!   Apache-2.0。`include_bytes!` でバイナリに埋め込む)を入れ、続いて
//!   `font_revision_ids` が指す font アセットを**指定順**に入れる。さらに
//!   serif / sans-serif / monospace / cursive / fantasy の 5 つの総称ファミリと
//!   `Options::font_family`(未知の `font-family` に対する既定)を**すべて同梱書体の
//!   ファミリ名に固定**し、`languages` を `["en"]` に固定する。
//!   したがってフォント選択は「同梱書体 + 明示的に渡されたアセット」の閉じた集合内で
//!   決まり、ホスト環境は結果に一切入らない。
//!
//! グリフを持たない文字(例: 同梱 Roboto だけで日本語を描こうとした場合)は、
//! 同梱書体の `.notdef`(いわゆる豆腐の □)として描かれる。依頼した文字は出ないのに
//! 画素は埋まるので、出力からは気づけない。そこで描画の前に `<text>` 配下の
//! **全子孫**テキストノードの文字を集め、DB 内の全フェイスの文字マップ
//! (skrifa の `charmap`)と突き合わせ、欠落した文字の種類数を警告にする。
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
/// 貼り付け位置 `x` / `y` の絶対値上限(validate)。
///
/// `x` / `y` は「画像の外へ出た分はクリップする」という仕様上、負値も含めて自由に
/// 取れるが、`i64` を無制限に受けると `apply` の `x + rx` が桁あふれして
/// debug ビルドで panic する。最大画素数(100MP ⇒ 1 辺は高々 1e8 未満)より広い
/// 1e8 を上限にすれば、実用上の配置(画像の外へ大きく逃がす)は全て表現でき、
/// かつ加算が `i64` の範囲に収まる。
const MAX_OFFSET: i64 = 100_000_000;
/// ラスタ画素数の上限(実行時)。`Limits::max_pixels` の既定と同じ 100MP。
const MAX_RASTER_PIXELS: u64 = 100_000_000;

/// `<text>` を含む SVG に対する実行時警告(文言はテストで固定する)。
pub(crate) const TEXT_WARNING: &str =
    "svg contains text elements; text is not rendered (convert text to paths for \
     deterministic output)";

/// 同梱書体: Roboto Regular(Apache-2.0、googlefonts/roboto v2.138)。
///
/// バイナリに埋め込む(`include_bytes!`)ので、実行環境にフォントが 1 本も
/// 入っていなくても `render_text: true` は同じ結果を出す。告知は
/// リポジトリ直下の `THIRD_PARTY_NOTICES.md` と `assets/fonts/LICENSE-Roboto.txt`。
const BUNDLED_FONT: &[u8] = include_bytes!("../../assets/fonts/Roboto-Regular.ttf");

/// 同梱書体のファミリ名が取れなかった場合の保険(通常は name テーブルから取る)。
const BUNDLED_FONT_FALLBACK_FAMILY: &str = "Roboto";

/// font アセット 1 本のバイト上限(32MiB)。CJK 書体(5〜20MB)を通せる幅。
pub const MAX_FONT_BYTES: u64 = 32 * 1024 * 1024;

/// 1 つの `svg_overlay` に渡せる font アセットの本数上限。
pub(crate) const MAX_FONT_ASSETS: usize = 4;

/// `svg_overlay` の静的検証(入力バイト列に依存しない制約のみ)。
///
/// 引数は DSL のフィールドをそのまま平らに受ける(op ごとの validate の共通作法)。
/// 専用の構造体を作ると recipe.rs の enum バリアントと二重定義になるので、
/// clippy の引数個数上限はここでは緩める。
#[allow(clippy::too_many_arguments)]
pub fn validate(
    index: usize,
    svg_revision_id: &str,
    x: i64,
    y: i64,
    opacity: f64,
    width: Option<u32>,
    height: Option<u32>,
    font_revision_ids: &[String],
) -> Result<()> {
    if font_revision_ids.len() > MAX_FONT_ASSETS {
        return Err(AtxError::InvalidRecipe(format!(
            "operations[{index}] (svg_overlay): font_revision_ids may hold at most \
             {MAX_FONT_ASSETS} entries, got {}",
            font_revision_ids.len()
        )));
    }
    for (k, id) in font_revision_ids.iter().enumerate() {
        if id.is_empty() {
            return Err(AtxError::InvalidRecipe(format!(
                "operations[{index}] (svg_overlay): font_revision_ids[{k}] must not be empty"
            )));
        }
        if !id.starts_with("rev_") {
            return Err(AtxError::InvalidRecipe(format!(
                "operations[{index}] (svg_overlay): font_revision_ids[{k}] must start with \
                 \"rev_\", got {id:?}"
            )));
        }
    }
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
    for (name, v) in [("x", x), ("y", y)] {
        if v.unsigned_abs() > MAX_OFFSET as u64 {
            return Err(AtxError::InvalidRecipe(format!(
                "operations[{index}] (svg_overlay): {name} must be within \
                 -{MAX_OFFSET}..={MAX_OFFSET} (the overlay is clipped to the image anyway; \
                 wider offsets only overflow the placement arithmetic), got {v}"
            )));
        }
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

/// `<image>` が外部ファイルを参照している SVG に対する実行時警告(文言はテストで固定する)。
pub(crate) const EXTERNAL_IMAGE_WARNING: &str =
    "svg contains image elements that reference external files; external references are \
     never loaded (embed the image in the SVG, or import it and composite it with layers)";

/// 決定論のためのフォント無し・外部参照無し usvg オプション。
///
/// `Options::default()` のフォント DB は**空**(`system-fonts` を付けていないので
/// システムフォントを読む経路がそもそもビルドに入っていない)。空 DB では
/// どの `font-family` も解決できず `<text>` は描画されない = v0.8 からの既定の挙動。
///
/// # `<image href>` の解決(セキュリティ点検で閉じた経路)
///
/// usvg の既定の文字列リゾルバは href を**ローカルファイルパス**とみなして
/// `std::fs::read` する。SVG アセットは信頼できない入力なので、これを許すと
/// サーバが動くマシンの任意のファイルを読みに行ける(`/dev/zero` で無制限のメモリ確保、
/// FIFO でハング、読めたローカル SVG は出力へ描き込まれる = 情報漏えい)。さらに出力が
/// ファイルシステムの状態に依存するので、決定論も壊れる。よって**文字列 href は常に
/// 解決しない**(`None` = その `<image>` は描かれない)。
///
/// `data:` URI は既定のリゾルバのまま許す。中身は SVG バイト列そのものに埋め込まれて
/// いるので自己完結・決定論的であり、大きさも SVG アセットのバイト上限に縛られる。
/// 入れ子の SVG は同じ `Options`(= この文字列リゾルバ)で解析されるため、
/// data: の中から外部ファイルを参照し直す抜け道も無い。なお PNG / JPEG 等のラスタは
/// `raster-images` 機能を外しているので、data: でも描画はされない(従来どおり)。
fn options() -> usvg::Options<'static> {
    let mut opt = usvg::Options::default();
    opt.image_href_resolver.resolve_string = Box::new(|_, _| None);
    opt
}

/// `render_text: true` のときに使うフォント群。
///
/// `extra` は `font_revision_ids` の**指定順**に並んだ font アセットのバイト列
/// (engine が `AssetResolver` から読み、[`validate_font_asset`] を通したもの)。
/// 同梱書体は常に先頭に入るので、ここには含まれない。
/// バイト列は `Arc` で共有する(`fontdb` へ渡すときも複製しない)。
#[derive(Debug, Default)]
pub(crate) struct TextFonts {
    pub extra: Vec<std::sync::Arc<Vec<u8>>>,
}

/// 同梱書体だけを載せた fontdb と、そのファミリ名。プロセスで 1 回だけ構築する。
///
/// 以前は op ごとに `Database::new()` + `load_font_data(BUNDLED_FONT.to_vec())` で、
/// 349KB の Roboto を複製してから再パースしていた。`Database` の clone は
/// `Source::Binary(Arc<..>)` の参照を増やすだけなので、バイト列はプロセス全体で 1 本、
/// フェイスの解析も 1 回で済む。
///
/// 総称ファミリ 5 種をここで同梱書体に固定しておく(clone が引き継ぐ)。これにより
/// SVG が `font-family="sans-serif"` と書いていても解決先は常に DB の中の書体になり、
/// ホスト環境は結果に入らない。
fn bundled_db() -> &'static (usvg::fontdb::Database, String) {
    static BUNDLED: std::sync::OnceLock<(usvg::fontdb::Database, String)> =
        std::sync::OnceLock::new();
    BUNDLED.get_or_init(|| {
        // 必ず**空の DB から**組み立てる(usvg の既定がいつか何かを読み込むように
        // なっても「入っているのは同梱書体と渡されたアセットだけ」を保てるように)。
        let mut db = usvg::fontdb::Database::new();
        db.load_font_source(usvg::fontdb::Source::Binary(std::sync::Arc::new(
            BUNDLED_FONT,
        )));
        // 同梱書体のファミリ名は fontdb 自身が name テーブルから読んだものを使う
        // (usvg の family 照合はこの文字列で行われるため、自前の推測とずれない)。
        let family = db
            .faces()
            .next()
            .and_then(|f| f.families.first().map(|(name, _)| name.clone()))
            .unwrap_or_else(|| BUNDLED_FONT_FALLBACK_FAMILY.to_string());
        db.set_serif_family(family.clone());
        db.set_sans_serif_family(family.clone());
        db.set_monospace_family(family.clone());
        db.set_cursive_family(family.clone());
        db.set_fantasy_family(family.clone());
        (db, family)
    })
}

/// 文字描画用の usvg オプション。[`options`] に「満たした fontdb」を足したもの。
///
/// 総称ファミリ 5 種と `font_family`(未知の family に対する既定)を全部
/// **同梱書体のファミリ名**に固定する。これにより SVG が
/// `font-family="Nonexistent"` や `font-family="sans-serif"` と書いていても、
/// 解決先は常に「DB の中にある書体」になり、ホスト環境は結果に入らない。
fn text_options(fonts: &TextFonts) -> usvg::Options<'static> {
    let mut opt = options();
    let (bundled, bundled_family) = bundled_db();
    let db = opt.fontdb_mut();
    // 同梱書体入りの DB を clone して、渡された font アセットを**指定順**に足す。
    // clone も load_font_source も Arc の参照を増やすだけで、バイト列は複製しない。
    *db = bundled.clone();
    for bytes in &fonts.extra {
        db.load_font_source(usvg::fontdb::Source::Binary(bytes.clone()));
    }
    opt.font_family = bundled_family.clone();
    // 言語依存の整形(ロケール別グリフ選択)を固定する。
    opt.languages = vec!["en".to_string()];
    opt
}

/// `<text>` 要素の中の文字のうち、DB のどのフェイスにもグリフが無いものの**種類数**。
///
/// グリフの無い文字は、同梱書体の `.notdef`(いわゆる豆腐の □)として描かれる
/// = 依頼した文字は出ないが、画素からはそれが分からない。だから描画の**前に**自分で
/// 数えて呼び出し元へ伝える。判定は skrifa の `charmap`(= usvg の整形器 harfrust が
/// 使うのと同じパーサ)。空白類は欠落しても見た目に出ないので数えない。
///
/// 走査対象は `<text>` 配下の**全子孫**テキストノード。`<tspan>` だけでなく
/// `<textPath>` / `<a>` / `<tref>` に包まれた文字も描かれるので、直接の子だけを見ると
/// CJK が豆腐になっても警告が 0 件になる。
fn missing_glyph_chars(doc: &usvg::roxmltree::Document, db: &usvg::fontdb::Database) -> usize {
    // 種類数だけを返すので集合で持つ(`Vec::contains` は総文字数 × 種類数かかる)。
    // `BTreeSet` なので反復順も決定論的。
    let mut chars: std::collections::BTreeSet<char> = std::collections::BTreeSet::new();
    for text_element in doc
        .descendants()
        .filter(|n| n.is_element() && n.has_tag_name("text"))
    {
        for node in text_element.descendants().filter(|n| n.is_text()) {
            chars.extend(
                node.text()
                    .unwrap_or("")
                    .chars()
                    .filter(|c| !c.is_whitespace()),
            );
        }
    }
    if chars.is_empty() {
        return 0;
    }
    // フェイスごとに「まだ見つかっていない文字」を消していく。
    // 反復順は `faces()`(挿入順の Vec)なので決定論的。
    let ids: Vec<_> = db.faces().map(|f| f.id).collect();
    for id in ids {
        if chars.is_empty() {
            break;
        }
        db.with_face_data(id, |data, index| {
            if let Ok(font) = skrifa::FontRef::from_index(data, index) {
                let charmap = skrifa::MetadataProvider::charmap(&font);
                chars.retain(|ch| charmap.map(*ch).is_none());
            }
        });
    }
    chars.len()
}

/// グリフ欠落の警告文(文言はテストで固定する)。
fn missing_glyph_warning(n: usize) -> String {
    format!(
        "svg text uses {n} character(s) with no glyph in the loaded fonts \
         (import a font asset and pass font_revision_ids)"
    )
}

/// font アセットの検証結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FontAssetInfo {
    /// SVG の `font-family` に書ける名前(fontdb が name テーブルから読んだもの。
    /// 1 ファイルに複数フェイスがある場合は全フェイス分、重複を除いて挿入順)。
    pub families: Vec<String>,
    /// 先頭フェイスのグリフ数(`maxp.numGlyphs`)。
    /// [`validate_font_asset`] が 0 を拒否した後の値なので必ず 1 以上。
    pub glyph_count: u16,
}

/// font アセットとして取り込んでよいバイト列かを検証する。
///
/// `import_asset`(MCP 層)と `svg_overlay` の実行時(engine)の**両方**が通す入口。
/// 受け入れる形式は単一フェイスの TrueType / OpenType のみ:
///
/// - 先頭 4 バイトが `00 01 00 00`(TrueType)/ `OTTO`(CFF アウトライン)/ `true`(旧 Mac)
/// - skrifa(usvg の整形器と同じパーサ)で解析でき、`maxp` と name テーブルがある
/// - `maxp.numGlyphs` が 1 以上(グリフを持たないフォントは 1 文字も描けないので、
///   渡されても「全部豆腐」になるだけ。理由が見える位置で弾く)
/// - `ttcf`(フォントコレクション)は**拒否**する: 「どのフェイスを使うか」が
///   レシピに表せず決定論的に指せないため。1 フェイスを取り出して渡してもらう
///
/// エラーは英語の平文(MCP 層がそのまま利用者に見せる。`validate_svg_asset` /
/// `validate_cube_asset` と同じ規約)。
pub fn validate_font_asset(bytes: &[u8]) -> std::result::Result<FontAssetInfo, String> {
    if bytes.len() as u64 > MAX_FONT_BYTES {
        return Err(format!(
            "the font is {} bytes, over the {MAX_FONT_BYTES} byte limit for a font asset",
            bytes.len()
        ));
    }
    let magic = bytes.get(..4).ok_or(
        "the file is too short to be a font (expected a TrueType or OpenType header)".to_string(),
    )?;
    if magic == b"ttcf" {
        return Err(
            "this is a TrueType/OpenType collection (.ttc); font collections are not supported; \
             extract one face and import that single font file"
                .to_string(),
        );
    }
    if !matches!(magic, [0x00, 0x01, 0x00, 0x00] | b"OTTO" | b"true") {
        return Err(
            "the file does not start with a TrueType or OpenType signature (expected \
             00 01 00 00, \"OTTO\" or \"true\"); .woff / .woff2 are not supported"
                .to_string(),
        );
    }
    let font =
        skrifa::FontRef::new(bytes).map_err(|e| format!("the font is not parseable: {e}"))?;
    let glyph_count = skrifa::raw::TableProvider::maxp(&font)
        .map_err(|e| format!("the font has no usable maxp table: {e}"))?
        .num_glyphs();
    if glyph_count == 0 {
        return Err(
            "the font declares no glyphs (maxp.numGlyphs is 0), so it cannot draw any character"
                .to_string(),
        );
    }

    // family 名は fontdb に読ませる(usvg の family 照合と同じ文字列になる)。
    let mut db = usvg::fontdb::Database::new();
    db.load_font_data(bytes.to_vec());
    let mut families: Vec<String> = Vec::new();
    for face in db.faces() {
        for (name, _) in &face.families {
            if !families.contains(name) {
                families.push(name.clone());
            }
        }
    }
    if families.is_empty() {
        return Err(
            "the font has no family name in its name table, so it cannot be referenced from a \
             font-family in the SVG"
                .to_string(),
        );
    }
    Ok(FontAssetInfo {
        families,
        glyph_count,
    })
}

/// ルート配下に「外部参照の `<image>`」(href が `data:` 以外)があるか。
fn has_external_image(doc: &usvg::roxmltree::Document) -> bool {
    const XLINK: &str = "http://www.w3.org/1999/xlink";
    doc.descendants()
        .filter(|n| n.is_element() && n.has_tag_name("image"))
        .filter_map(|n| n.attribute("href").or_else(|| n.attribute((XLINK, "href"))))
        .any(|href| !href.trim_start().starts_with("data:"))
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
    validate_asset(bytes).ok().flatten()
}

/// SVG アセットとして取り込んでよいバイト列かを検証し、固有サイズを返す。
///
/// `svg_overlay` の [`rasterize`] が要求するのと同じ条件(UTF-8・XML として解析できる・
/// ルート要素が `<svg>`・usvg が描画ツリーを組める)を import 時に先取りする。
/// これが無いと、拡張子が `.svg` なだけの任意内容のファイルが台帳に載り、
/// export の上書きで任意の場所へ書き出す「ファイル複写」の経路になっていた
/// (セキュリティ点検、DESIGN.md §9.14)。
///
/// - `Ok(Some((w, h)))`: 検証 OK、固有サイズあり
/// - `Ok(None)`: 検証 OK、固有サイズなし(svg_overlay 側で width/height が必要)
/// - `Err(reason)`: SVG アセットとして不正(英語の理由。MCP 層がそのまま載せる)
pub fn validate_asset(bytes: &[u8]) -> std::result::Result<Option<(u32, u32)>, String> {
    let text = source_text(bytes)
        .ok_or("the file is not valid UTF-8 text (gzipped .svgz is not supported)")?;
    let doc = usvg::roxmltree::Document::parse(text)
        .map_err(|e| format!("the file is not parseable XML: {e}"))?;
    let root = doc.root_element();
    if !root.has_tag_name("svg") {
        return Err(format!(
            "the root element is <{}>, not <svg>",
            root.tag_name().name()
        ));
    }
    let tree = usvg::Tree::from_xmltree(&doc, &options())
        .map_err(|e| format!("the file is not a renderable SVG: {e}"))?;
    if !has_intrinsic_size(&root) {
        return Ok(None);
    }
    let size = tree.size();
    Ok(round_positive(size.width() as f64).zip(round_positive(size.height() as f64)))
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
/// `text` が `None`(= `render_text: false`)ならフォント DB は空で `<text>` は
/// 描画されない。`Some` なら同梱書体 + 渡された font アセットで文字を描く
/// (モジュール冒頭の「決定論とフォント」を参照)。
///
/// 失敗は呼び出し側(engine)が `AtxError::Operation` へ包む前提の平文メッセージで返す。
pub(crate) fn rasterize(
    bytes: &[u8],
    width: Option<u32>,
    height: Option<u32>,
    text: Option<&TextFonts>,
) -> std::result::Result<Raster, String> {
    let text_source = source_text(bytes).ok_or_else(|| {
        "the referenced asset is not valid UTF-8 XML; svg_overlay needs a plain (non-gzipped) \
         .svg file"
            .to_string()
    })?;
    let doc = usvg::roxmltree::Document::parse(text_source)
        .map_err(|e| format!("the referenced asset is not parseable XML: {e}"))?;
    let root = doc.root_element();
    if !root.has_tag_name("svg") {
        return Err(format!(
            "the referenced asset's root element is <{}>, not <svg>",
            root.tag_name().name()
        ));
    }
    let intrinsic = has_intrinsic_size(&root);
    // `<text>` が 1 つも無い SVG では fontdb を組まない(描く文字が無いので出力は同じ)。
    let has_text_element = doc
        .descendants()
        .any(|n| n.is_element() && n.has_tag_name("text"));
    let opt = match text {
        Some(fonts) if has_text_element => text_options(fonts),
        _ => options(),
    };
    let mut warnings = Vec::new();
    match text {
        // 文字描画あり: グリフ被覆を描画前に数える
        // (欠落した文字は .notdef の □ になるので、画素からは気づけない)。
        Some(_) if has_text_element => {
            let missing = missing_glyph_chars(&doc, &opt.fontdb);
            if missing > 0 {
                warnings.push(missing_glyph_warning(missing));
            }
        }
        Some(_) => {}
        // 文字描画なし: フォントを一切読まないので `<text>` は描画されない
        // (モジュール冒頭の設計note)。判定は素朴な文字列走査で十分
        // (誤検出しても警告が 1 本増えるだけ)。
        None => {
            if text_source.contains("<text") {
                warnings.push(TEXT_WARNING.to_string());
            }
        }
    }
    let tree = usvg::Tree::from_xmltree(&doc, &opt)
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

    if has_external_image(&doc) {
        warnings.push(EXTERNAL_IMAGE_WARNING.to_string());
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
/// - 画像の外へ出た部分は**クリップ**する(負の座標も可)。位置の加算は
///   `saturating_add`(validate が `|x|, |y| <= MAX_OFFSET` を保証しているので
///   本来飽和しないが、算術の安全性を validate だけに依存させない)
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
        let iy = y.saturating_add(ry as i64);
        if iy < 0 || iy >= ch as i64 {
            continue;
        }
        for rx in 0..rw {
            let ix = x.saturating_add(rx as i64);
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
        let err = rasterize(svg.as_bytes(), None, None, None).unwrap_err();
        assert!(err.contains("no intrinsic size"), "{err}");
        // 両方指定すれば通る。
        assert!(rasterize(svg.as_bytes(), Some(4), Some(4), None).is_ok());
    }

    #[test]
    fn not_svg_and_not_xml_are_distinct_errors() {
        assert!(intrinsic_size(b"\x89PNG\r\n").is_none());
        let err = rasterize(b"not xml at all", None, None, None).unwrap_err();
        assert!(err.contains("not parseable XML"), "{err}");
        let err = rasterize(b"<html><body/></html>", None, None, None).unwrap_err();
        assert!(err.contains("not <svg>"), "{err}");
    }

    /// 幅だけ指定すると縦横比が保たれる(8x4 → 幅 16 なら高さ 8)。
    #[test]
    fn width_only_preserves_the_aspect_ratio() {
        let r = rasterize(BADGE.as_bytes(), Some(16), None, None).unwrap();
        assert_eq!(r.img.dimensions(), (16, 8));
        let r = rasterize(BADGE.as_bytes(), None, Some(8), None).unwrap();
        assert_eq!(r.img.dimensions(), (16, 8));
    }

    /// 不透明な赤い矩形はストレートアルファで厳密に (1, 0, 0, 1) になる。
    #[test]
    fn opaque_fill_unpremultiplies_exactly() {
        let r = rasterize(BADGE.as_bytes(), None, None, None).unwrap();
        assert_eq!(r.img.dimensions(), (8, 4));
        assert_eq!(r.img.get(4, 2), [1.0, 0.0, 0.0, 1.0]);
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn text_elements_raise_a_warning() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="4"><text x="0" y="3">hi</text></svg>"#;
        let r = rasterize(svg.as_bytes(), None, None, None).unwrap();
        assert_eq!(r.warnings, vec![TEXT_WARNING.to_string()]);
    }

    #[test]
    fn validate_matrix() {
        assert!(validate(0, "rev_x", 0, 0, 1.0, None, None, &[]).is_ok());
        assert!(validate(0, "", 0, 0, 1.0, None, None, &[]).is_err());
        assert!(validate(0, "nope", 0, 0, 1.0, None, None, &[]).is_err());
        assert!(validate(0, "rev_x", 0, 0, 1.5, None, None, &[]).is_err());
        assert!(validate(0, "rev_x", 0, 0, f64::NAN, None, None, &[]).is_err());
        assert!(validate(0, "rev_x", 0, 0, 1.0, Some(0), None, &[]).is_err());
        assert!(validate(0, "rev_x", 0, 0, 1.0, None, Some(0), &[]).is_err());
        assert!(validate(0, "rev_x", 0, 0, 1.0, Some(MAX_RASTER_EDGE + 1), None, &[]).is_err());
    }

    /// 回帰: 青天井の `x` / `y` は validate で弾く。
    ///
    /// 以前は範囲検査が無く、`x = i64::MAX` が `apply` の `x + rx` を桁あふれさせて
    /// debug ビルドで panic していた(release では巻き戻って別の場所へ描かれる)。
    #[test]
    fn validate_rejects_unbounded_offsets() {
        for (x, y) in [
            (i64::MAX, 0),
            (i64::MIN, 0),
            (0, i64::MAX),
            (MAX_OFFSET + 1, 0),
            (0, -MAX_OFFSET - 1),
        ] {
            let err = validate(0, "rev_x", x, y, 1.0, None, None, &[])
                .expect_err("out-of-range offset must be rejected");
            assert!(err.to_string().contains("svg_overlay"), "{err}");
        }
        // 上限ちょうどは通る(画像の外へ大きく逃がす配置は仕様の範囲内)。
        assert!(validate(0, "rev_x", -MAX_OFFSET, MAX_OFFSET, 1.0, None, None, &[]).is_ok());
    }

    /// 回帰: 上限いっぱいの負オフセットでも桁あふれせず、全部クリップされるだけ。
    #[test]
    fn extreme_negative_placement_clips_without_overflow() {
        let mut img = LinearImage::from_pixel(4, 4, [0.0, 0.0, 0.0, 1.0]);
        let raster = LinearImage::from_pixel(4, 4, [1.0, 1.0, 1.0, 1.0]);
        apply(
            &mut img,
            &raster,
            -MAX_OFFSET,
            -MAX_OFFSET,
            BlendMode::Normal,
            1.0,
        );
        assert_eq!(img.get(0, 0), [0.0, 0.0, 0.0, 1.0]);
        // 飽和加算なので i64::MAX でも panic しない(validate の後段の belt-and-braces)。
        apply(
            &mut img,
            &raster,
            i64::MAX,
            i64::MAX,
            BlendMode::Normal,
            1.0,
        );
        assert_eq!(img.get(3, 3), [0.0, 0.0, 0.0, 1.0]);
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
