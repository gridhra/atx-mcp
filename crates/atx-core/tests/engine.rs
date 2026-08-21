//! 変換エンジンのテスト(DESIGN §6「ゴールデンテスト」「決定論」)。
//!
//! 写真らしい合成フィクスチャ `tests/fixtures/synthetic_scene.jpg`
//! (`cargo run -p atx-core --example gen_fixture` で生成)と、
//! テスト内で生成する小さな合成画像を併用する。

use atx_core::recipe::TransformRecipe;
use atx_core::{apply_recipe, inspect_bytes, Limits};
use image::{ImageFormat, Rgba, RgbaImage};

const FIXTURE: &[u8] = include_bytes!("../../../tests/fixtures/synthetic_scene.jpg");

fn recipe(json: &str) -> TransformRecipe {
    serde_json::from_str(json).expect("recipe should parse")
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

/// 決定論的な合成画像(斜めグラデーション + 市松のアクセント)。
fn synthetic(w: u32, h: u32, alpha: bool) -> RgbaImage {
    RgbaImage::from_fn(w, h, |x, y| {
        let r = ((x * 255) / w.max(1)) as u8;
        let g = ((y * 255) / h.max(1)) as u8;
        let b = if (x / 8 + y / 8) % 2 == 0 { 40 } else { 200 };
        let a = if alpha { (x % 256) as u8 } else { 255 };
        Rgba([r, g, b, a])
    })
}

fn encode_png(img: &RgbaImage) -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::Png).unwrap();
    out.into_inner()
}

fn dims(bytes: &[u8]) -> (u32, u32) {
    let info = inspect_bytes(bytes, &Limits::default()).unwrap();
    (info.width, info.height)
}

/// Orientation タグだけを持つ最小の EXIF APP1 セグメントを SOI 直後に差し込む。
/// (EXIF 書き込みクレートを使わずにテスト用の入力を組み立てるためのヘルパ)
fn jpeg_with_orientation(img: &RgbaImage, orientation: u16) -> Vec<u8> {
    let mut jpeg = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img.clone())
        .to_rgb8()
        .write_to(&mut jpeg, ImageFormat::Jpeg)
        .unwrap();
    insert_exif_app1(&jpeg.into_inner(), orientation)
}

/// 既存 JPEG バイト列の SOI 直後に Orientation のみの EXIF APP1 を注入する。
/// フィクスチャは EXIF を一切持たないため、
/// EXIF 処理経路のテストはこのヘルパで EXIF を付与して行う。
fn insert_exif_app1(jpeg: &[u8], orientation: u16) -> Vec<u8> {
    // TIFF ヘッダ(ビッグエンディアン) + IFD0(エントリ1件: 0x0112 Orientation, SHORT)
    let mut tiff: Vec<u8> = Vec::new();
    tiff.extend_from_slice(b"MM\x00\x2a");
    tiff.extend_from_slice(&8u32.to_be_bytes()); // IFD0 のオフセット
    tiff.extend_from_slice(&1u16.to_be_bytes()); // エントリ数
    tiff.extend_from_slice(&0x0112u16.to_be_bytes()); // Orientation
    tiff.extend_from_slice(&3u16.to_be_bytes()); // type = SHORT
    tiff.extend_from_slice(&1u32.to_be_bytes()); // count
    tiff.extend_from_slice(&orientation.to_be_bytes()); // 値(4 バイト枠の先頭 2 バイト)
    tiff.extend_from_slice(&[0, 0]);
    tiff.extend_from_slice(&0u32.to_be_bytes()); // 次の IFD なし

    let mut payload = b"Exif\x00\x00".to_vec();
    payload.extend_from_slice(&tiff);

    let mut out = jpeg[0..2].to_vec(); // SOI
    out.extend_from_slice(&[0xFF, 0xE1]);
    out.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(&payload);
    out.extend_from_slice(&jpeg[2..]);
    out
}

/// EXIF Orientation は inspect で報告され、変換時は必ず画素へ焼き込まれる。
#[test]
fn exif_orientation_is_normalized_into_pixels() {
    // orientation=6 は「時計回りに 90° 回すと正立」の意味 → 実効寸法は縦横入れ替え。
    let input = jpeg_with_orientation(&synthetic(80, 40, false), 6);

    let info = inspect_bytes(&input, &Limits::default()).unwrap();
    assert_eq!(info.exif_orientation, Some(6));
    assert_eq!((info.width, info.height), (80, 40));
    assert_eq!((info.oriented_width, info.oriented_height), (40, 80));

    // auto_orient を書かなくても正規化される(EXIF を落とす以上、常に適用する)。
    let out = apply_recipe(
        &input,
        &recipe(r#"{"operations":[{"op":"encode","format":"png"}]}"#),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!((out.width, out.height), (40, 80));

    // auto_orient を明示しても結果は同一(実質 no-op)。
    let explicit = apply_recipe(
        &input,
        &recipe(r#"{"operations":[{"op":"auto_orient"},{"op":"encode","format":"png"}]}"#),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(explicit.bytes, out.bytes);

    // 出力に Orientation は残らない。
    assert_eq!(
        inspect_bytes(&out.bytes, &Limits::default())
            .unwrap()
            .exif_orientation,
        None
    );
}

// ---------------------------------------------------------------- inspect

#[test]
fn inspect_fixture_reports_real_metadata() {
    let info = inspect_bytes(FIXTURE, &Limits::default()).unwrap();
    assert_eq!((info.width, info.height), (1477, 1108));
    assert_eq!(info.mime_type, "image/jpeg");
    assert_eq!(info.byte_size, FIXTURE.len() as u64);
    assert!(!info.has_alpha, "baseline jpeg has no alpha channel");
    assert!(info.has_icc_profile, "fixture carries an APP2 ICC profile");
    // フィクスチャは完全合成で、EXIF セグメントを一切持たない(APP2 ICC のみ)。
    // EXIF なし = None / 空 / GPS なし を正として固定する。
    assert_eq!(info.exif_orientation, None);
    assert_eq!(
        (info.oriented_width, info.oriented_height),
        (info.width, info.height)
    );
    assert!(info.exif_summary.is_empty());
    assert!(!info.has_gps, "fixture must never carry GPS (public repo)");
}

#[test]
fn inspect_synthetic_png_reports_alpha() {
    let bytes = encode_png(&synthetic(20, 10, true));
    let info = inspect_bytes(&bytes, &Limits::default()).unwrap();
    assert_eq!((info.width, info.height), (20, 10));
    assert_eq!(info.mime_type, "image/png");
    assert!(info.has_alpha);
    assert!(!info.has_icc_profile);
    assert_eq!(info.exif_orientation, None);
    assert!(!info.has_gps);
}

#[test]
fn inspect_enforces_byte_limit_before_decoding() {
    let limits = Limits {
        max_bytes: 1024,
        ..Default::default()
    };
    let err = inspect_bytes(FIXTURE, &limits).unwrap_err();
    assert!(matches!(err, atx_core::AtxError::LimitExceeded(_)), "{err}");
    assert!(err.to_string().contains("limit is 1024 bytes"));
}

#[test]
fn inspect_enforces_pixel_limit() {
    let limits = Limits {
        max_pixels: 1000,
        ..Default::default()
    };
    let err = inspect_bytes(FIXTURE, &limits).unwrap_err();
    assert!(matches!(err, atx_core::AtxError::LimitExceeded(_)), "{err}");
}

#[test]
fn inspect_rejects_non_image_bytes() {
    let err = inspect_bytes(b"not an image at all", &Limits::default()).unwrap_err();
    assert!(matches!(err, atx_core::AtxError::Decode(_)), "{err}");
}

// ------------------------------------------------------------ determinism

/// 同一入力 + 同一レシピ → バイト同一出力(DESIGN §5)。
fn assert_deterministic(input: &[u8], json: &str) -> Vec<u8> {
    let r = recipe(json);
    let a = apply_recipe(input, &r, &Limits::default()).unwrap();
    let b = apply_recipe(input, &r, &Limits::default()).unwrap();
    assert_eq!(a.bytes, b.bytes, "output bytes must be identical: {json}");
    assert_eq!((a.width, a.height), (b.width, b.height));
    assert_eq!(a.warnings, b.warnings);
    a.bytes
}

#[test]
fn deterministic_jpeg_output() {
    let out = assert_deterministic(
        FIXTURE,
        r#"{"operations":[
            {"op":"auto_orient"},
            {"op":"resize","width":640,"fit":"contain"},
            {"op":"encode","format":"jpeg","quality":85}
        ]}"#,
    );
    assert_eq!(&out[0..2], &[0xFF, 0xD8], "jpeg SOI marker");
}

#[test]
fn deterministic_png_output() {
    let input = encode_png(&synthetic(64, 48, true));
    let out = assert_deterministic(
        &input,
        r#"{"operations":[
            {"op":"crop","aspect_ratio":"1:1"},
            {"op":"encode","format":"png"}
        ]}"#,
    );
    assert_eq!(&out[1..4], b"PNG");
    assert_eq!(dims(&out), (48, 48));
}

#[test]
fn deterministic_webp_output() {
    let out = assert_deterministic(
        FIXTURE,
        r#"{"operations":[
            {"op":"resize","width":320,"height":180,"fit":"cover"},
            {"op":"encode","format":"webp","quality":82}
        ]}"#,
    );
    assert_eq!(&out[0..4], b"RIFF");
    assert_eq!(&out[8..12], b"WEBP");
}

#[test]
fn deterministic_avif_output() {
    // AVIF (rav1e) は重いので極小画像で。スレッド数固定により決定論を担保している。
    let input = encode_png(&synthetic(32, 32, false));
    let out = assert_deterministic(
        &input,
        r#"{"operations":[{"op":"encode","format":"avif","quality":80}]}"#,
    );
    assert_eq!(&out[4..8], b"ftyp", "ISOBMFF box");
}

/// Encode op が無い場合は入力フォーマットを維持する。
#[test]
fn without_encode_op_keeps_input_format() {
    let out = apply_recipe(
        FIXTURE,
        &recipe(r#"{"operations":[{"op":"resize","width":100,"fit":"contain"}]}"#),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(out.mime_type, "image/jpeg");

    let png = encode_png(&synthetic(20, 20, true));
    let out = apply_recipe(
        &png,
        &recipe(r#"{"operations":[{"op":"resize","width":10,"fit":"contain"}]}"#),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(out.mime_type, "image/png");
}

// ---------------------------------------------------------------- rotate

#[test]
fn rotate_largest_inscribed_rect_shrinks_canvas() {
    let input = encode_png(&synthetic(200, 100, false));
    let out = apply_recipe(
        &input,
        &recipe(
            r#"{"operations":[
                {"op":"rotate","angle_degrees":10.0,"crop":"largest_inscribed_rect"},
                {"op":"encode","format":"png"}
            ]}"#,
        ),
        &Limits::default(),
    )
    .unwrap();

    // 内接矩形は必ず元より小さい。
    assert!(
        out.width < 200 && out.height < 100,
        "{}x{}",
        out.width,
        out.height
    );
    // 200x100 を 10° 回転したときの最大内接矩形の理論値は約 191.1 x 67.8。
    assert!((out.width as i64 - 191).abs() <= 1, "width {}", out.width);
    assert!((out.height as i64 - 67).abs() <= 1, "height {}", out.height);
    // 残存画素は 6 割強
    let kept = (out.width as f64 * out.height as f64) / (200.0 * 100.0);
    assert!((0.60..0.68).contains(&kept), "kept {kept}");
    assert!(
        out.warnings
            .iter()
            .any(|w| w.contains("removed") && w.contains("% of pixels")),
        "{:?}",
        out.warnings
    );
}

#[test]
fn rotate_full_expands_canvas() {
    let input = encode_png(&synthetic(200, 100, false));
    let out = apply_recipe(
        &input,
        &recipe(
            r#"{"operations":[
                {"op":"rotate","angle_degrees":90.0,"crop":"full"},
                {"op":"encode","format":"png"}
            ]}"#,
        ),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!((out.width, out.height), (100, 200));
}

/// 0°(と ±360°)は再サンプリングを行わないため、無変換と等価。
#[test]
fn rotate_zero_is_identity() {
    let input = encode_png(&synthetic(40, 30, false));
    let rotated = apply_recipe(
        &input,
        &recipe(
            r#"{"operations":[{"op":"rotate","angle_degrees":0.0},{"op":"encode","format":"png"}]}"#,
        ),
        &Limits::default(),
    )
    .unwrap();
    let plain = apply_recipe(
        &input,
        &recipe(r#"{"operations":[{"op":"encode","format":"png"}]}"#),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(rotated.bytes, plain.bytes);
}

// ---------------------------------------------------------------- resize

fn resized(input: &[u8], resize_json: &str) -> (u32, u32) {
    let out = apply_recipe(
        input,
        &recipe(&format!(
            r#"{{"operations":[{resize_json},{{"op":"encode","format":"png"}}]}}"#
        )),
        &Limits::default(),
    )
    .unwrap();
    (out.width, out.height)
}

#[test]
fn resize_fit_semantics() {
    let input = encode_png(&synthetic(400, 200, false)); // 2:1

    // cover: ボックスを覆ってから中央クロップ → 指定寸法ちょうど
    assert_eq!(
        resized(
            &input,
            r#"{"op":"resize","width":100,"height":100,"fit":"cover"}"#
        ),
        (100, 100)
    );
    // contain: ボックスに収まる(クロップなし)
    assert_eq!(
        resized(
            &input,
            r#"{"op":"resize","width":100,"height":100,"fit":"contain"}"#
        ),
        (100, 50)
    );
    // fill: 比率無視
    assert_eq!(
        resized(
            &input,
            r#"{"op":"resize","width":100,"height":100,"fit":"fill"}"#
        ),
        (100, 100)
    );
    // 片辺のみ指定 → 比率維持
    assert_eq!(
        resized(&input, r#"{"op":"resize","width":100,"fit":"cover"}"#),
        (100, 50)
    );
    assert_eq!(
        resized(&input, r#"{"op":"resize","height":100,"fit":"contain"}"#),
        (200, 100)
    );
}

#[test]
fn resize_without_enlargement_is_respected() {
    let input = encode_png(&synthetic(100, 50, false));
    // 既定 (without_enlargement=true) では拡大しない
    assert_eq!(
        resized(&input, r#"{"op":"resize","width":400,"fit":"contain"}"#),
        (100, 50)
    );
    // 明示的に false にすれば拡大する
    assert_eq!(
        resized(
            &input,
            r#"{"op":"resize","width":400,"fit":"contain","without_enlargement":false}"#
        ),
        (400, 200)
    );
    // fill でも同様
    assert_eq!(
        resized(
            &input,
            r#"{"op":"resize","width":400,"height":400,"fit":"fill"}"#
        ),
        (100, 50)
    );
}

// ------------------------------------------------------------------ crop

#[test]
fn crop_aspect_ratio_trims_to_ratio() {
    let input = encode_png(&synthetic(400, 200, false)); // 2:1

    // 16:9 は 2:1 より縦長 → 幅が詰められる
    let (w, h) = resized(&input, r#"{"op":"crop","aspect_ratio":"16:9"}"#);
    assert_eq!((w, h), (356, 200));
    assert!(((w as f64 / h as f64) - 16.0 / 9.0).abs() < 0.01);

    // 1:1 → 正方形
    assert_eq!(
        resized(&input, r#"{"op":"crop","aspect_ratio":"1:1"}"#),
        (200, 200)
    );
    // 3:1 は 2:1 より横長 → 高さが詰められる。
    // 高さは round(400/3)=133 になるが、そのままだと 400/133 = 3.0075 で比率が
    // target を跨いでしまう(＝再適用で今度は幅が動く)。寸法計算は不動点まで
    // 反復するので、1回の適用で幅も 399 (= 133*3、ちょうど 3:1)に収まる。
    assert_eq!(
        resized(&input, r#"{"op":"crop","aspect_ratio":"3:1"}"#),
        (399, 133)
    );
}

/// アスペクト比クロップは1回の適用で不動点に到達する(2回目は恒等)。
///
/// 旧実装の反例: 8x8 に "1:6" を適用すると (1,8)(比率 1:8)になり、
/// 2回目の適用で分岐が反転して (1,6) に動いていた。
#[test]
fn crop_aspect_ratio_is_idempotent_for_8x8_1_6() {
    let input = encode_png(&synthetic(8, 8, false));
    let once = resized(&input, r#"{"op":"crop","aspect_ratio":"1:6"}"#);
    assert_eq!(once, (1, 6));

    let twice = apply_recipe(
        &input,
        &recipe(
            r#"{"operations":[
                {"op":"crop","aspect_ratio":"1:6"},
                {"op":"crop","aspect_ratio":"1:6"},
                {"op":"encode","format":"png"}
            ]}"#,
        ),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!((twice.width, twice.height), (1, 6));
}

#[test]
fn crop_pad_mode_adds_borders() {
    let input = encode_png(&synthetic(400, 200, false));
    let out = apply_recipe(
        &input,
        &recipe(
            r##"{"operations":[
                {"op":"crop","aspect_ratio":"1:1","mode":"pad","pad_color":"#000000"},
                {"op":"encode","format":"png"}
            ]}"##,
        ),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!((out.width, out.height), (400, 400));

    let decoded = image::load_from_memory(&out.bytes).unwrap().to_rgba8();
    // 上端は pad 色(黒)、中央は元画像
    assert_eq!(decoded.get_pixel(0, 0).0[0..3], [0, 0, 0]);
    assert_ne!(decoded.get_pixel(200, 200).0[0..3], [0, 0, 0]);
}

#[test]
fn crop_anchor_selects_region() {
    let input = encode_png(&synthetic(400, 200, false));
    let left = apply_recipe(
        &input,
        &recipe(
            r#"{"operations":[{"op":"crop","aspect_ratio":"1:1","anchor":"left"},{"op":"encode","format":"png"}]}"#,
        ),
        &Limits::default(),
    )
    .unwrap();
    let right = apply_recipe(
        &input,
        &recipe(
            r#"{"operations":[{"op":"crop","aspect_ratio":"1:1","anchor":"right"},{"op":"encode","format":"png"}]}"#,
        ),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!((left.width, left.height), (200, 200));
    assert_eq!((right.width, right.height), (200, 200));
    assert_ne!(
        left.bytes, right.bytes,
        "anchor must change the cropped region"
    );
}

#[test]
fn crop_rect_out_of_bounds_is_an_error() {
    let input = encode_png(&synthetic(40, 30, false));
    let err = apply_recipe(
        &input,
        &recipe(r#"{"operations":[{"op":"crop","rect":{"x":30,"y":0,"width":20,"height":10}}]}"#),
        &Limits::default(),
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("out of bounds"), "{msg}");
    assert!(msg.contains("operation 0 (crop)"), "{msg}");
}

#[test]
fn crop_rect_within_bounds() {
    let input = encode_png(&synthetic(40, 30, false));
    assert_eq!(
        resized(
            &input,
            r#"{"op":"crop","rect":{"x":5,"y":5,"width":20,"height":10}}"#
        ),
        (20, 10)
    );
}

// ------------------------------------------- crop (coordinate_space: source)

/// マーカー矩形の色(背景のグラデーションには絶対に現れない色)。
const MARKER: [u8; 3] = [255, 0, 255];

/// 背景グラデーション + 指定座標に単色マーカー矩形を置いた合成画像。
fn with_marker(w: u32, h: u32, m: (u32, u32, u32, u32)) -> RgbaImage {
    let (mx, my, mw, mh) = m;
    RgbaImage::from_fn(w, h, |x, y| {
        if x >= mx && x < mx + mw && y >= my && y < my + mh {
            Rgba([MARKER[0], MARKER[1], MARKER[2], 255])
        } else {
            // 緑成分だけを動かすグラデーション(マーカー色 (255,0,255) とは決して一致しない)。
            Rgba([20, (60 + (x + y) % 120) as u8, 20, 255])
        }
    })
}

fn decode(bytes: &[u8]) -> RgbaImage {
    image::load_from_memory(bytes).unwrap().to_rgba8()
}

fn close_to_marker(px: &Rgba<u8>, tol: i32) -> bool {
    (0..3).all(|i| (px.0[i] as i32 - MARKER[i] as i32).abs() <= tol)
}

/// rotate(内接矩形)で座標系がずれた後でも、SOURCE 座標の矩形がマーカーを切り出す。
#[test]
fn source_space_crop_follows_rotation() {
    // 300x200 の (100,60)-(160,100) にマーカー。
    let src = with_marker(300, 200, (100, 60, 60, 40));
    let input = encode_png(&src);

    let out = apply_recipe(
        &input,
        &recipe(
            r#"{"operations":[
                {"op":"rotate","angle_degrees":-1.8,"crop":"largest_inscribed_rect"},
                {"op":"crop","rect":{"x":100,"y":60,"width":60,"height":40},
                 "coordinate_space":"source"},
                {"op":"encode","format":"png"}
            ]}"#,
        ),
        &Limits::default(),
    )
    .unwrap();

    // 出力寸法は「回転させた矩形の外接矩形」= 60x40 よりわずかに大きい。
    // 理論値: 60*cos(1.8°) + 40*sin(1.8°) ≈ 61.2 / 40*cos(1.8°) + 60*sin(1.8°) ≈ 41.9
    assert!(
        (out.width as i64 - 61).abs() <= 1 && (out.height as i64 - 42).abs() <= 1,
        "mapped bbox {}x{}",
        out.width,
        out.height
    );

    let img = decode(&out.bytes);
    // 中心は間違いなくマーカーの内部。
    let center = *img.get_pixel(out.width / 2, out.height / 2);
    assert!(close_to_marker(&center, 2), "center pixel {center:?}");

    // 外接矩形なので四隅には背景が混じる(= 回転した四角形そのものではない)。
    let corner = *img.get_pixel(0, 0);
    assert!(!close_to_marker(&corner, 40), "corner pixel {corner:?}");

    // 同じ rect を current 座標系で切ると、回転ぶんだけ位置がずれる。
    let naive = apply_recipe(
        &input,
        &recipe(
            r#"{"operations":[
                {"op":"rotate","angle_degrees":-1.8,"crop":"largest_inscribed_rect"},
                {"op":"crop","rect":{"x":100,"y":60,"width":60,"height":40}},
                {"op":"encode","format":"png"}
            ]}"#,
        ),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!((naive.width, naive.height), (60, 40));
    assert_ne!(naive.bytes, out.bytes);
}

/// EXIF Orientation(6 = 時計回り 90° で正立)の正規化も座標系を動かす。
#[test]
fn source_space_crop_follows_exif_auto_orient() {
    // 200x120 の (20,10)-(60,40) にマーカー。orientation=6 で 120x200 に正規化される。
    let src = with_marker(200, 120, (20, 10, 40, 30));
    let input = jpeg_with_orientation(&src, 6);

    let out = apply_recipe(
        &input,
        &recipe(
            r#"{"operations":[
                {"op":"auto_orient"},
                {"op":"crop","rect":{"x":20,"y":10,"width":40,"height":30},
                 "coordinate_space":"source"},
                {"op":"encode","format":"png"}
            ]}"#,
        ),
        &Limits::default(),
    )
    .unwrap();

    // 90° 回転なので幅と高さが入れ替わる(外接矩形は厳密に一致)。
    assert_eq!((out.width, out.height), (30, 40));
    let img = decode(&out.bytes);
    let center = *img.get_pixel(out.width / 2, out.height / 2);
    // 入力が JPEG なので色は厳密一致しない。
    assert!(close_to_marker(&center, 24), "center pixel {center:?}");
}

/// resize を挟んでも SOURCE 座標はスケールされて追従する。
#[test]
fn source_space_crop_follows_resize() {
    // 400x200 の (200,80)-(280,120) にマーカー。1/4 に縮小 → (50,20)-(70,30)。
    let src = with_marker(400, 200, (200, 80, 80, 40));
    let input = encode_png(&src);

    let out = apply_recipe(
        &input,
        &recipe(
            r#"{"operations":[
                {"op":"resize","width":100,"fit":"contain"},
                {"op":"crop","rect":{"x":200,"y":80,"width":80,"height":40},
                 "coordinate_space":"source"},
                {"op":"encode","format":"png"}
            ]}"#,
        ),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!((out.width, out.height), (20, 10));

    let img = decode(&out.bytes);
    let center = *img.get_pixel(out.width / 2, out.height / 2);
    assert!(close_to_marker(&center, 4), "center pixel {center:?}");

    // 同じ結果を current 座標で書くと (50,20,20,10) になる。
    let manual = apply_recipe(
        &input,
        &recipe(
            r#"{"operations":[
                {"op":"resize","width":100,"fit":"contain"},
                {"op":"crop","rect":{"x":50,"y":20,"width":20,"height":10}},
                {"op":"encode","format":"png"}
            ]}"#,
        ),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(manual.bytes, out.bytes);
}

/// rotate90 + resize(cover) + crop の連鎖でも追従する。
#[test]
fn source_space_crop_follows_a_longer_chain() {
    let src = with_marker(240, 160, (40, 40, 80, 40));
    let input = encode_png(&src);

    let out = apply_recipe(
        &input,
        &recipe(
            r#"{"operations":[
                {"op":"rotate","angle_degrees":90.0},
                {"op":"resize","width":80,"height":80,"fit":"cover"},
                {"op":"crop","rect":{"x":40,"y":40,"width":80,"height":40},
                 "coordinate_space":"source"},
                {"op":"encode","format":"png"}
            ]}"#,
        ),
        &Limits::default(),
    )
    .unwrap();

    let img = decode(&out.bytes);
    let center = *img.get_pixel(out.width / 2, out.height / 2);
    assert!(
        close_to_marker(&center, 8),
        "center pixel {center:?} ({}x{})",
        out.width,
        out.height
    );
}

/// 幾何 op が一つも無ければ source と current は完全に等価。
#[test]
fn source_space_equals_current_space_without_geometry_ops() {
    let input = encode_png(&synthetic(64, 48, false));
    let rect = r#"{"x":7,"y":5,"width":21,"height":13}"#;

    let source = apply_recipe(
        &input,
        &recipe(&format!(
            r#"{{"operations":[
                {{"op":"adjust","brightness":0.1}},
                {{"op":"crop","rect":{rect},"coordinate_space":"source"}},
                {{"op":"encode","format":"png"}}
            ]}}"#
        )),
        &Limits::default(),
    )
    .unwrap();
    let current = apply_recipe(
        &input,
        &recipe(&format!(
            r#"{{"operations":[
                {{"op":"adjust","brightness":0.1}},
                {{"op":"crop","rect":{rect}}},
                {{"op":"encode","format":"png"}}
            ]}}"#
        )),
        &Limits::default(),
    )
    .unwrap();

    assert_eq!(source.bytes, current.bytes);
    assert_eq!(source.warnings, current.warnings);
    assert_eq!((source.width, source.height), (21, 13));
}

/// 画像外へはみ出した source 矩形はクランプされ、警告が出る。
#[test]
fn source_space_crop_clamps_and_warns() {
    let input = encode_png(&synthetic(100, 100, false));
    let out = apply_recipe(
        &input,
        &recipe(
            r#"{"operations":[
                {"op":"rotate","angle_degrees":10.0,"crop":"largest_inscribed_rect"},
                {"op":"crop","rect":{"x":0,"y":0,"width":100,"height":100},
                 "coordinate_space":"source"},
                {"op":"encode","format":"png"}
            ]}"#,
        ),
        &Limits::default(),
    )
    .unwrap();
    // 内接矩形は元より小さいので、source 全域を指すと必ずクランプされる。
    assert!(
        out.warnings.iter().any(|w| w.contains("was clamped to")),
        "{:?}",
        out.warnings
    );
    assert!(out.width <= 100 && out.height <= 100);
}

/// 現在の画像とまったく交差しない source 矩形は構造化エラー(写像後座標つき)。
#[test]
fn source_space_crop_with_empty_intersection_is_an_error() {
    let input = encode_png(&synthetic(200, 100, false));
    let err = apply_recipe(
        &input,
        &recipe(
            r#"{"operations":[
                {"op":"crop","rect":{"x":0,"y":0,"width":40,"height":40}},
                {"op":"crop","rect":{"x":120,"y":10,"width":40,"height":40},
                 "coordinate_space":"source"}
            ]}"#,
        ),
        &Limits::default(),
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("operation 1 (crop)"), "{msg}");
    assert!(msg.contains("does not intersect"), "{msg}");
    // 写像後の座標がメッセージに含まれる。
    assert!(msg.contains("maps to ["), "{msg}");
}

/// coordinate_space: source は rect 専用(aspect_ratio との併用は静的エラー)。
#[test]
fn source_space_requires_a_rect() {
    let err = atx_core::recipe::validate(&recipe(
        r#"{"operations":[{"op":"crop","aspect_ratio":"16:9","coordinate_space":"source"}]}"#,
    ))
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("only valid"), "{msg}");
    assert!(msg.contains("rect"), "{msg}");
}

// ---------------------------------------------------------------- adjust

#[test]
fn adjust_brightness_moves_pixels_predictably() {
    let flat = RgbaImage::from_pixel(8, 8, Rgba([100, 100, 100, 255]));
    let input = encode_png(&flat);
    let out = apply_recipe(
        &input,
        &recipe(
            r#"{"operations":[{"op":"adjust","brightness":0.1},{"op":"encode","format":"png"}]}"#,
        ),
        &Limits::default(),
    )
    .unwrap();
    let decoded = image::load_from_memory(&out.bytes).unwrap().to_rgba8();
    // 100 + 0.1*255 = 125.5 → 126(丸め)
    assert_eq!(decoded.get_pixel(0, 0).0[0..3], [126, 126, 126]);
}

#[test]
fn adjust_zero_values_are_identity() {
    let input = encode_png(&synthetic(32, 32, false));
    let adjusted = apply_recipe(
        &input,
        &recipe(
            r#"{"operations":[{"op":"adjust","brightness":0.0,"contrast":0.0,"saturation":0.0,"sharpness":0.0},{"op":"encode","format":"png"}]}"#,
        ),
        &Limits::default(),
    )
    .unwrap();
    let plain = apply_recipe(
        &input,
        &recipe(r#"{"operations":[{"op":"encode","format":"png"}]}"#),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(adjusted.bytes, plain.bytes);
}

// -------------------------------------------------------------- metadata

#[test]
fn exif_drop_is_reported_as_a_warning() {
    // フィクスチャは EXIF レスなので、EXIF を注入した入力で経路を検証する。
    let input = insert_exif_app1(FIXTURE, 1);
    let out = apply_recipe(
        &input,
        &recipe(r#"{"operations":[{"op":"resize","width":64,"fit":"contain"},{"op":"encode","format":"jpeg"}]}"#),
        &Limits::default(),
    )
    .unwrap();
    assert!(
        out.warnings
            .iter()
            .any(|w| w.contains("EXIF metadata was dropped")),
        "{:?}",
        out.warnings
    );
    // 出力側に EXIF は残っていない
    let info = inspect_bytes(&out.bytes, &Limits::default()).unwrap();
    assert_eq!(info.exif_orientation, None);
    assert!(info.exif_summary.is_empty());
}

/// 既定では JPEG 出力に ICC を温存する。
#[test]
fn icc_profile_is_preserved_for_jpeg_output() {
    let out = apply_recipe(
        FIXTURE,
        &recipe(r#"{"operations":[{"op":"resize","width":64,"fit":"contain"},{"op":"encode","format":"jpeg"}]}"#),
        &Limits::default(),
    )
    .unwrap();
    let info = inspect_bytes(&out.bytes, &Limits::default()).unwrap();
    assert!(info.has_icc_profile, "ICC should survive a jpeg re-encode");
}

/// strip_metadata(all) は ICC も落とし、出力にメタデータが残らないことを保証する。
#[test]
fn strip_metadata_all_removes_icc_too() {
    let out = apply_recipe(
        FIXTURE,
        &recipe(
            r#"{"operations":[
                {"op":"resize","width":64,"fit":"contain"},
                {"op":"strip_metadata","scope":"all"},
                {"op":"encode","format":"jpeg"}
            ]}"#,
        ),
        &Limits::default(),
    )
    .unwrap();
    let info = inspect_bytes(&out.bytes, &Limits::default()).unwrap();
    assert!(!info.has_icc_profile);
    assert_eq!(info.exif_orientation, None);
    assert!(
        out.warnings
            .iter()
            .any(|w| w.contains("strip_metadata(all)")),
        "{:?}",
        out.warnings
    );
}

/// PNG 出力では ICC を埋め込まない(v1 の割り切り)ため warning が出る。
#[test]
fn icc_profile_is_dropped_for_non_jpeg_output() {
    let out = apply_recipe(
        FIXTURE,
        &recipe(r#"{"operations":[{"op":"resize","width":64,"fit":"contain"},{"op":"encode","format":"png"}]}"#),
        &Limits::default(),
    )
    .unwrap();
    assert!(
        out.warnings
            .iter()
            .any(|w| w.contains("ICC profile dropped")),
        "{:?}",
        out.warnings
    );
}

/// scope=gps は v1 では既定動作(EXIF 全体破棄)と同じ。その旨を warning で明示する。
#[test]
fn strip_metadata_gps_documents_v1_behavior() {
    let out = apply_recipe(
        FIXTURE,
        &recipe(
            r#"{"operations":[
                {"op":"resize","width":64,"fit":"contain"},
                {"op":"strip_metadata","scope":"gps"},
                {"op":"encode","format":"jpeg"}
            ]}"#,
        ),
        &Limits::default(),
    )
    .unwrap();
    assert!(
        out.warnings
            .iter()
            .any(|w| w.contains("strip_metadata(gps)")),
        "{:?}",
        out.warnings
    );
    // ICC は温存されたまま
    assert!(
        inspect_bytes(&out.bytes, &Limits::default())
            .unwrap()
            .has_icc_profile
    );
}

/// scope=exif は EXIF(GPS 含む)を確実に落としつつ ICC を温存する。
#[test]
fn strip_metadata_exif_preserves_icc() {
    // 前提: ICC(フィクスチャ由来)と EXIF(注入)の両方を持つ入力を組み立てる。
    let input = insert_exif_app1(FIXTURE, 1);
    let src = inspect_bytes(&input, &Limits::default()).unwrap();
    assert!(src.has_icc_profile && !src.exif_summary.is_empty());

    let out = apply_recipe(
        &input,
        &recipe(
            r#"{"operations":[
                {"op":"resize","width":64,"fit":"contain"},
                {"op":"strip_metadata","scope":"exif"},
                {"op":"encode","format":"jpeg"}
            ]}"#,
        ),
        &Limits::default(),
    )
    .unwrap();

    let info = inspect_bytes(&out.bytes, &Limits::default()).unwrap();
    assert!(info.has_icc_profile, "exif scope must keep the ICC profile");
    assert_eq!(info.exif_orientation, None);
    assert!(info.exif_summary.is_empty());
    assert!(!info.has_gps);
    // 出力バイト列に EXIF APP1 マーカーが残っていないこと。
    assert!(
        !out.bytes.windows(6).any(|w| w == b"Exif\x00\x00"),
        "no EXIF payload may remain in the output"
    );

    // 警告は整合的: exif スコープの説明があり、ICC 破棄の警告は出ない。
    assert!(
        out.warnings
            .iter()
            .any(|w| w.contains("strip_metadata(exif)") && w.contains("ICC profile is preserved")),
        "{:?}",
        out.warnings
    );
    assert!(
        !out.warnings
            .iter()
            .any(|w| w.contains("ICC profile removed") || w.contains("ICC profile dropped")),
        "{:?}",
        out.warnings
    );
}

/// PNG 出力では exif スコープでも ICC は埋め込めない(従来どおり警告付きで破棄)。
#[test]
fn strip_metadata_exif_still_drops_icc_for_png_output() {
    let out = apply_recipe(
        FIXTURE,
        &recipe(
            r#"{"operations":[
                {"op":"resize","width":64,"fit":"contain"},
                {"op":"strip_metadata","scope":"exif"},
                {"op":"encode","format":"png"}
            ]}"#,
        ),
        &Limits::default(),
    )
    .unwrap();
    assert!(
        out.warnings
            .iter()
            .any(|w| w.contains("ICC profile dropped")),
        "{:?}",
        out.warnings
    );
    assert!(
        !inspect_bytes(&out.bytes, &Limits::default())
            .unwrap()
            .has_icc_profile
    );
}

// --------------------------------------------------- hash stability (new fields)

/// 新フィールド `coordinate_space` は既定値のとき正規化 JSON に現れない。
/// = 既存レシピの `recipe_hash` はバイト単位で不変(ゴールデン参照)。
#[test]
fn coordinate_space_default_is_invisible_to_the_hash() {
    let without =
        recipe(r#"{"operations":[{"op":"crop","rect":{"x":1,"y":2,"width":3,"height":4}}]}"#);
    let explicit_default = recipe(
        r#"{"operations":[{"op":"crop","rect":{"x":1,"y":2,"width":3,"height":4},
            "coordinate_space":"current"}]}"#,
    );
    let source = recipe(
        r#"{"operations":[{"op":"crop","rect":{"x":1,"y":2,"width":3,"height":4},
            "coordinate_space":"source"}]}"#,
    );

    let canon = atx_core::canonical_json(&without).unwrap();
    assert!(
        !canon.contains("coordinate_space"),
        "default must not appear in canonical JSON: {canon}"
    );
    assert_eq!(canon, atx_core::canonical_json(&explicit_default).unwrap());
    assert_eq!(
        atx_core::recipe_hash(&without).unwrap(),
        atx_core::recipe_hash(&explicit_default).unwrap()
    );

    // source は意味が違うので当然ハッシュも変わり、かつ往復で安定する。
    assert_ne!(
        atx_core::recipe_hash(&without).unwrap(),
        atx_core::recipe_hash(&source).unwrap()
    );
    let json = atx_core::canonical_json(&source).unwrap();
    assert!(json.contains("\"coordinate_space\":\"source\""), "{json}");
    let back: TransformRecipe = serde_json::from_str(&json).unwrap();
    assert_eq!(
        atx_core::recipe_hash(&back).unwrap(),
        atx_core::recipe_hash(&source).unwrap()
    );
}

/// StripScope の variant 追加も既存ハッシュを動かさない。
#[test]
fn strip_scope_hashes_are_stable() {
    // 既存の 2 値のハッシュは従来のまま(ピン留め)。
    assert_eq!(
        atx_core::recipe_hash(&recipe(
            r#"{"operations":[{"op":"strip_metadata","scope":"all"}]}"#
        ))
        .unwrap(),
        atx_core::recipe_hash(&recipe(r#"{"operations":[{"op":"strip_metadata"}]}"#)).unwrap(),
        "all is the default scope"
    );
    let exif = recipe(r#"{"operations":[{"op":"strip_metadata","scope":"exif"}]}"#);
    let json = atx_core::canonical_json(&exif).unwrap();
    assert_eq!(
        json,
        r#"{"operations":[{"op":"strip_metadata","scope":"exif"}]}"#
    );
    let back: TransformRecipe = serde_json::from_str(&json).unwrap();
    assert_eq!(
        atx_core::recipe_hash(&back).unwrap(),
        atx_core::recipe_hash(&exif).unwrap()
    );
}

/// source 座標クロップを含むレシピも決定論的(バイト同一)。
#[test]
fn source_space_crop_is_deterministic() {
    let input = encode_png(&with_marker(160, 120, (30, 20, 50, 40)));
    assert_deterministic(
        &input,
        r#"{"operations":[
            {"op":"rotate","angle_degrees":-3.5,"crop":"largest_inscribed_rect"},
            {"op":"crop","rect":{"x":30,"y":20,"width":50,"height":40},"coordinate_space":"source"},
            {"op":"encode","format":"png"}
        ]}"#,
    );
}

// ---------------------------------------------------------------- golden

/// ゴールデンテスト: フルパイプラインの出力 sha256 を固定する。
///
/// このハッシュは `ENGINE_VERSION` 時点のエンジン挙動(各 op のアルゴリズム、
/// エンコーダの設定、依存クレートのバージョン)をピン留めしている。
/// 意図的に挙動を変えた場合のみ、`ENGINE_VERSION` を上げた上でこの値を更新すること。
#[test]
fn golden_full_pipeline_sha256() {
    let r = recipe(
        r#"{"operations":[
            {"op":"auto_orient"},
            {"op":"rotate","angle_degrees":-1.8,"crop":"largest_inscribed_rect"},
            {"op":"crop","aspect_ratio":"16:9","anchor":"center"},
            {"op":"resize","width":800,"fit":"cover"},
            {"op":"adjust","brightness":0.05,"contrast":0.02,"saturation":0.03,"sharpness":0.2},
            {"op":"encode","format":"jpeg","quality":85}
        ]}"#,
    );
    let out = apply_recipe(FIXTURE, &r, &Limits::default()).unwrap();
    assert_eq!(atx_core::ENGINE_VERSION, "atx-core/2");
    assert_eq!((out.width, out.height), (800, 450));
    // 出力ハッシュは合成フィクスチャ `tests/fixtures/synthetic_scene.jpg`
    // (`cargo run -p atx-core --example gen_fixture` で再生成可能)に対してピン留めしている。
    // フィクスチャを作り直した場合はこの値も必ず更新すること。
    //
    // 履歴: 以前は個人所有の実写フィクスチャに対する値
    // (99b05d96ec3b99ad82af5786857766982e4883342f93a758b5e5d07b3821d0c0、
    //  さらにその前の 446cfd82... はバグのある JPEG エンコード設定
    //  `set_optimized_huffman_tables(true)` 時代のもの。codec::encode_jpeg のドキュメント参照)。
    // 今回の変更は入力画像の差し替えのみでエンジン挙動は不変なので、
    // ENGINE_VERSION は DESIGN §9-7 の方針どおり据え置く。
    assert_eq!(
        sha256_hex(&out.bytes),
        // v2 (f32 linear) golden; v1 value was bc05827c9622122dc94f64ff9ae735aa682c4320541fcffd61465d30dfc67e2a
        "b9371140e7251bd3ea2b92e46e7a3c02aa4d32e6fc07224ff7eb33f6512d15f8"
    );

    // レシピハッシュも同時にピン留めする(冪等キーの回帰検証)。
    assert_eq!(
        atx_core::recipe_hash(&r).unwrap(),
        "884ea169e1027cf26d9140f6d2f7543904b2ca344667640f87820f528eaa175d"
    );
}
