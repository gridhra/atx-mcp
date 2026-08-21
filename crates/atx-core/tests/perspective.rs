//! `perspective` op(v0.2)のテスト。
//!
//! 検証対象:
//! - validate の排他性・凸性・値域チェック
//! - 決定論(同一入力 → バイト同一出力)
//! - キーストーンの往復(+θ → -θ)が内部領域で恒等に戻ること
//! - quad 形式が四角形を軸並行長方形へ直すこと(行ごとの黒画素幅で判定)
//! - perspective 越しの `coordinate_space: "source"` クロップ
//! - フルパイプラインのゴールデン sha256

use atx_core::recipe::TransformRecipe;
use atx_core::{apply_recipe, Limits};
use image::{ImageFormat, Rgba, RgbaImage};

const FIXTURE: &[u8] = include_bytes!("../../../tests/fixtures/synthetic_scene.jpg");

fn recipe(json: &str) -> TransformRecipe {
    serde_json::from_str(json).expect("recipe should parse")
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

fn encode_png(img: &RgbaImage) -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::Png).unwrap();
    out.into_inner()
}

fn decode(bytes: &[u8]) -> RgbaImage {
    image::load_from_memory(bytes)
        .expect("output should decode")
        .to_rgba8()
}

/// 滑らかな合成画像(bicubic 補間の誤差評価がしやすいよう高周波成分を持たせない)。
fn gradient(w: u32, h: u32) -> RgbaImage {
    RgbaImage::from_fn(w, h, |x, y| {
        let r = (30 + (x * 180) / w.max(1)) as u8;
        let g = (40 + (y * 170) / h.max(1)) as u8;
        let b = (60 + ((x + y) * 120) / (w + h).max(1)) as u8;
        Rgba([r, g, b, 255])
    })
}

/// 点が四角形(tl, tr, br, bl)の内部にあるか(全辺に対して同じ側)。
fn inside_quad(q: &[[f64; 2]; 4], x: f64, y: f64) -> bool {
    (0..4).all(|i| {
        let a = q[i];
        let b = q[(i + 1) % 4];
        (b[0] - a[0]) * (y - a[1]) - (b[1] - a[1]) * (x - a[0]) >= 0.0
    })
}

/// 白背景に四角形 `q` を黒で塗った画像。
fn quad_marker(w: u32, h: u32, q: &[[f64; 2]; 4]) -> RgbaImage {
    RgbaImage::from_fn(w, h, |x, y| {
        if inside_quad(q, x as f64 + 0.5, y as f64 + 0.5) {
            Rgba([0, 0, 0, 255])
        } else {
            Rgba([255, 255, 255, 255])
        }
    })
}

/// 行 `y` に含まれる暗い画素(黒マーカー)の数。
fn dark_run(img: &RgbaImage, y: u32) -> u32 {
    (0..img.width())
        .filter(|x| img.get_pixel(*x, y).0[0] < 128)
        .count() as u32
}

// ------------------------------------------------------------- validate

fn validate_err(op_json: &str) -> String {
    let r = recipe(&format!(r#"{{"operations":[{op_json}]}}"#));
    atx_core::recipe::validate(&r)
        .expect_err("this perspective op must be rejected")
        .to_string()
}

/// 2 形式の併用は拒否する。
#[test]
fn validate_rejects_both_forms() {
    let err = validate_err(
        r#"{"op":"perspective","quad":[[0,0],[10,0],[10,10],[0,10]],"vertical_degrees":5}"#,
    );
    assert!(err.contains("exactly one form"), "{err}");
}

/// どちらの形式も指定しないのは拒否する。
#[test]
fn validate_rejects_neither_form() {
    let err = validate_err(r#"{"op":"perspective"}"#);
    assert!(err.contains("one form is required"), "{err}");
}

/// 凹四角形・自己交差・退化・順序違いはすべて拒否する。
#[test]
fn validate_rejects_non_convex_quads() {
    // 凹(br が内側にめり込んでいる)
    let concave = validate_err(r#"{"op":"perspective","quad":[[0,0],[100,0],[50,50],[0,100]]}"#);
    assert!(concave.contains("strictly convex"), "{concave}");

    // 自己交差(tr と br が入れ替わった蝶ネクタイ)
    let bowtie = validate_err(r#"{"op":"perspective","quad":[[0,0],[100,100],[100,0],[0,100]]}"#);
    assert!(bowtie.contains("strictly convex"), "{bowtie}");

    // 3 点が一直線(退化)
    let degenerate = validate_err(r#"{"op":"perspective","quad":[[0,0],[50,0],[100,0],[0,100]]}"#);
    assert!(degenerate.contains("strictly convex"), "{degenerate}");

    // 反時計回り(bl, br, tr, tl の順)は「順序違い」として拒否する
    let reversed = validate_err(r#"{"op":"perspective","quad":[[0,100],[100,100],[100,0],[0,0]]}"#);
    assert!(reversed.contains("strictly convex"), "{reversed}");
}

/// 非有限座標・角度の値域・pad_color の書式。
#[test]
fn validate_rejects_bad_scalars() {
    let angle = validate_err(r#"{"op":"perspective","vertical_degrees":45.5}"#);
    assert!(angle.contains("vertical_degrees"), "{angle}");
    let angle_h = validate_err(r#"{"op":"perspective","horizontal_degrees":-60}"#);
    assert!(angle_h.contains("horizontal_degrees"), "{angle_h}");
    let color = validate_err(r#"{"op":"perspective","vertical_degrees":5,"pad_color":"blue"}"#);
    assert!(color.contains("pad_color"), "{color}");
}

/// 正しい 2 形式は通る(境界値 ±45° を含む)。
#[test]
fn validate_accepts_both_valid_forms() {
    for json in [
        r#"{"op":"perspective","quad":[[10,10],[90,20],[95,90],[5,80]]}"#,
        r##"{"op":"perspective","vertical_degrees":45,"horizontal_degrees":-45,"pad_color":"#000"}"##,
        r#"{"op":"perspective","vertical_degrees":0}"#,
    ] {
        let r = recipe(&format!(r#"{{"operations":[{json}]}}"#));
        atx_core::recipe::validate(&r).unwrap_or_else(|e| panic!("{json} should be valid: {e}"));
    }
}

// ---------------------------------------------------------- determinism

/// 同一入力・同一レシピは 2 回実行してもバイト同一。
#[test]
fn perspective_is_deterministic() {
    let input = encode_png(&gradient(160, 120));
    let r = recipe(
        r#"{"operations":[
            {"op":"perspective","vertical_degrees":9.5,"horizontal_degrees":-3.25},
            {"op":"encode","format":"png"}
        ]}"#,
    );
    let a = apply_recipe(&input, &r, &Limits::default()).unwrap();
    let b = apply_recipe(&input, &r, &Limits::default()).unwrap();
    assert_eq!(sha256_hex(&a.bytes), sha256_hex(&b.bytes));

    let q = recipe(
        r#"{"operations":[
            {"op":"perspective","quad":[[12,9],[150,20],[140,110],[20,101]]},
            {"op":"encode","format":"png"}
        ]}"#,
    );
    let c = apply_recipe(&input, &q, &Limits::default()).unwrap();
    let d = apply_recipe(&input, &q, &Limits::default()).unwrap();
    assert_eq!(sha256_hex(&c.bytes), sha256_hex(&d.bytes));
}

/// キーストーン補正はキャンバス寸法を変えず、パディング率を警告する。
#[test]
fn keystone_keeps_canvas_and_reports_padding() {
    let input = encode_png(&gradient(200, 150));
    let r = recipe(
        r#"{"operations":[
            {"op":"perspective","vertical_degrees":12},
            {"op":"encode","format":"png"}
        ]}"#,
    );
    let out = apply_recipe(&input, &r, &Limits::default()).unwrap();
    assert_eq!((out.width, out.height), (200, 150));
    let warn = out
        .warnings
        .iter()
        .find(|w| w.contains("padding"))
        .unwrap_or_else(|| panic!("padding warning expected, got {:?}", out.warnings));
    assert!(warn.starts_with("operations[0] (perspective): "), "{warn}");
    assert!(
        warn.contains("% of the 200x150 output is padding"),
        "{warn}"
    );

    // パディングは既定で白。上辺が広がる = 下辺は縮むので、余白は下隅に出る。
    let img = decode(&out.bytes);
    assert_eq!(img.get_pixel(0, 149).0, [255, 255, 255, 255]);
    assert_eq!(img.get_pixel(199, 149).0, [255, 255, 255, 255]);
}

/// 正の `vertical_degrees` は上辺を広げる(= 上すぼまりの補正)。
///
/// 上すぼまりの台形マーカーを +θ で補正すると、上側の行の黒画素幅が
/// 補正前より増える(下側は減る)ことで符号の意味を確認する。
#[test]
fn positive_vertical_widens_the_top() {
    let q = [[70.0, 20.0], [130.0, 20.0], [160.0, 180.0], [40.0, 180.0]];
    let src = quad_marker(200, 200, &q);
    let before_top = dark_run(&src, 30);
    let before_bottom = dark_run(&src, 170);

    let input = encode_png(&src);
    let r = recipe(
        r#"{"operations":[
            {"op":"perspective","vertical_degrees":20},
            {"op":"encode","format":"png"}
        ]}"#,
    );
    let out = decode(&apply_recipe(&input, &r, &Limits::default()).unwrap().bytes);
    let after_top = dark_run(&out, 30);
    let after_bottom = dark_run(&out, 170);

    assert!(
        after_top > before_top,
        "top should be widened: {before_top} -> {after_top}"
    );
    assert!(
        after_bottom < before_bottom,
        "bottom should be narrowed: {before_bottom} -> {after_bottom}"
    );
}

/// キーストーンの往復(+θ → -θ)は内部領域でほぼ恒等に戻る。
///
/// モデル上、縦キーストーンの合成は角度パラメータ `tan θ` の加算なので
/// `H(-θ) ∘ H(θ)` は厳密に恒等行列になる。残差は bicubic 補間 2 回ぶんだけ。
#[test]
fn keystone_round_trip_is_near_identity() {
    let original = gradient(200, 150);
    let input = encode_png(&original);
    let forward = apply_recipe(
        &input,
        &recipe(
            r#"{"operations":[
                {"op":"perspective","vertical_degrees":8},
                {"op":"encode","format":"png"}
            ]}"#,
        ),
        &Limits::default(),
    )
    .unwrap();
    let back = apply_recipe(
        &forward.bytes,
        &recipe(
            r#"{"operations":[
                {"op":"perspective","vertical_degrees":-8},
                {"op":"encode","format":"png"}
            ]}"#,
        ),
        &Limits::default(),
    )
    .unwrap();
    let round = decode(&back.bytes);
    assert_eq!(round.dimensions(), original.dimensions());

    // 端はパディングで欠けるので中央 60% の領域だけを比較する。
    let (w, h) = original.dimensions();
    let (x0, x1) = (w / 5, w * 4 / 5);
    let (y0, y1) = (h / 5, h * 4 / 5);
    let mut max_diff = 0i32;
    for y in y0..y1 {
        for x in x0..x1 {
            for c in 0..3 {
                let d = round.get_pixel(x, y).0[c] as i32 - original.get_pixel(x, y).0[c] as i32;
                max_diff = max_diff.max(d.abs());
            }
        }
    }
    assert!(max_diff <= 6, "round trip drifted by {max_diff} levels");
}

// ------------------------------------------------------------ quad 形式

/// quad 形式: 台形マーカーが軸並行長方形に直る。
///
/// 出力寸法は「平均辺長の保存」規則どおり。補正後は全行の黒画素幅が
/// 出力幅と一致する(= 左右の辺が垂直になった)ことを確認する。
/// 補正前は上辺 60px / 下辺 120px と大きく食い違っているので、非自明な検査になる。
#[test]
fn quad_form_rectifies_the_marker() {
    let q = [[70.0, 20.0], [130.0, 20.0], [160.0, 180.0], [40.0, 180.0]];
    let src = quad_marker(200, 200, &q);
    assert_eq!(dark_run(&src, 21), 60, "top edge of the source trapezoid");
    assert_eq!(
        dark_run(&src, 178),
        120,
        "bottom edge of the source trapezoid"
    );

    let input = encode_png(&src);
    let out = apply_recipe(
        &input,
        &recipe(
            r#"{"operations":[
                {"op":"perspective","quad":[[70,20],[130,20],[160,180],[40,180]]},
                {"op":"encode","format":"png"}
            ]}"#,
        ),
        &Limits::default(),
    )
    .unwrap();

    // 平均辺長: 幅 = (60 + 120) / 2 = 90、高さ = (|(40,180)-(70,20)| + |(160,180)-(130,20)|) / 2
    //          = sqrt(30^2 + 160^2) = 162.788... → 163
    assert_eq!((out.width, out.height), (90, 163));

    let img = decode(&out.bytes);
    // 4 隅(2px 内側)がマーカー色 = quad の 4 隅が出力の 4 隅へ落ちている。
    for (x, y) in [(2, 2), (87, 2), (87, 160), (2, 160)] {
        assert!(
            img.get_pixel(x, y).0[0] < 128,
            "corner ({x}, {y}) should be inside the marker, got {:?}",
            img.get_pixel(x, y).0
        );
    }
    // どの行でも黒が幅いっぱい(端の 1 列は bicubic で白と混ざりうる)に広がり、
    // かつ行ごとの幅が揃っている = 左右の辺が垂直になった。
    // 補正前は上辺 60px / 下辺 120px と 2 倍違っていたので、非自明な検査になる。
    let runs: Vec<u32> = [5, 40, 81, 120, 157]
        .iter()
        .map(|y| dark_run(&img, *y))
        .collect();
    let (lo, hi) = (*runs.iter().min().unwrap(), *runs.iter().max().unwrap());
    assert!(lo >= 87, "rows should span the full 90px width: {runs:?}");
    assert!(hi <= 90 && hi - lo <= 2, "rows should be uniform: {runs:?}");
}

/// perspective 越しの SOURCE 座標クロップ。
///
/// 元画像の既知座標に置いたマーカーを、quad 補正後に
/// `coordinate_space: "source"` の矩形で切り出せる(= 射影行列で追跡できている)。
#[test]
fn source_space_crop_survives_perspective() {
    let mut src = gradient(240, 180);
    // (100, 60) に 16x16 のマゼンタマーカー。
    for y in 60..76 {
        for x in 100..116 {
            src.put_pixel(x, y, Rgba([255, 0, 255, 255]));
        }
    }
    let input = encode_png(&src);

    let out = apply_recipe(
        &input,
        &recipe(
            r#"{"operations":[
                {"op":"perspective","quad":[[20,20],[220,10],[230,170],[10,160]]},
                {"op":"crop","rect":{"x":100,"y":60,"width":16,"height":16},
                 "coordinate_space":"source"},
                {"op":"encode","format":"png"}
            ]}"#,
        ),
        &Limits::default(),
    )
    .unwrap();

    // AABB 化でわずかに広がるが、マーカーとほぼ同寸で切り出せているはず。
    assert!(
        (14..=22).contains(&out.width) && (14..=22).contains(&out.height),
        "unexpected crop size {}x{}",
        out.width,
        out.height
    );
    let img = decode(&out.bytes);
    let center = img.get_pixel(img.width() / 2, img.height() / 2).0;
    assert!(
        center[0] > 200 && center[1] < 80 && center[2] > 200,
        "the source-space crop should land on the magenta marker, got {center:?}"
    );
}

// ---------------------------------------------------------------- golden

/// ゴールデン: フィクスチャ + perspective + jpeg エンコードの出力 sha256。
///
/// `ENGINE_VERSION` 時点の perspective の挙動(射影モデル・係数の 1e-6 量子化・
/// imageproc の bicubic warp・JPEG エンコーダ設定)をピン留めする。
/// 意図的に挙動を変えた場合のみ `ENGINE_VERSION` を上げた上で更新すること。
/// 入力は完全合成のフィクスチャ `tests/fixtures/synthetic_scene.jpg`
/// (`cargo run -p atx-core --example gen_fixture` で再生成可能)。
#[test]
fn golden_perspective_pipeline_sha256() {
    let r = recipe(
        r##"{"operations":[
            {"op":"perspective","vertical_degrees":6.5,"horizontal_degrees":-2.0,
             "pad_color":"#202020"},
            {"op":"encode","format":"jpeg","quality":85}
        ]}"##,
    );
    let out = apply_recipe(FIXTURE, &r, &Limits::default()).unwrap();
    assert_eq!(atx_core::ENGINE_VERSION, "atx-core/2");
    assert_eq!((out.width, out.height), (1477, 1108));
    assert_eq!(
        sha256_hex(&out.bytes),
        // v2 (f32 linear) golden; v1 value was 01b9590af0888b494f86cefd9a0b0f5db7061b48c183c9564cd3c43998c34ea9
        "3251829bfcade24e722443f197b199710d9c28c37a7054d24e4227c5f86f215c"
    );
}
