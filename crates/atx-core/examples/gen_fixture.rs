//! テストフィクスチャの生成器。
//!
//! - `tests/fixtures/synthetic_scene.jpg` … 合成の「建築写真」
//! - `evals/fixtures/tilted_scene.jpg` … 上を -2.4° 回した傾きフィクスチャ
//! - `tests/fixtures/synthetic_document.png` … 白い用紙 + 単語状バー + 一様な灰色余白
//! - `evals/fixtures/document_photo.jpg` … 上の用紙を暗い机に置き、キーストーンで歪めた「写真」
//! - `evals/fixtures/dark_ui_screenshot.png` … ダークモード UI のスクリーンショット風合成
//!
//! ```sh
//! cargo run -p atx-core --example gen_fixture
//! ```
//!
//! ## tilted_scene.jpg(evals/tasks/t01_straighten_eyecatch.json 用)
//!
//! `synthetic_scene.jpg` はほぼ水平(atx-geometry の傾き検出が ~0° を返す)なため、
//! 「まっすぐにして」という eval タスクで `rotate` op を使わなくても正しく振る舞える
//! 状態だった。これは eval タスク側の不備であり、フィクスチャを客観的に傾いた画像に
//! する方が正しい修正(docs/DESIGN.md 参照)。
//!
//! `synthetic_scene.jpg` と同じ合成シーンに対し、atx-core の決定論エンジン自身
//! (`apply_recipe`)で `Rotate { angle_degrees: -2.4, crop: largest_inscribed_rect }`
//! を適用したものを書き出す。atx-core の `Rotate.angle_degrees` は「正 = 時計回り」、
//! atx-geometry の `recommended_angle_degrees` も同じ規約(「正 = 時計回りに回すと
//! 水平になる」)なので、-2.4° 回転させた画像を水平に戻す補正角は理論上ちょうど
//! +2.4° になる。本生成器はこれを仮定で終わらせず、生成直後に
//! `atx_geometry::detect_tilt` を実際に走らせて `recommended_angle_degrees` が
//! +2.4° 近辺・十分な confidence であることを assert で検証する。
//!
//! ## 方針(DESIGN §9 フィクスチャ方針)
//!
//! リポジトリに第三者/個人の写真を置かないため、テスト用の「写真らしい」画像は
//! すべてこの生成器で合成する。乱数源は固定シードの LCG のみで、`rand` も時刻も
//! 使わない。したがって何度実行してもバイト同一の JPEG が得られる
//! (この例自身が 2 回エンコードして一致を検証する)。
//!
//! ## 描くもの(パイプライン全体を動かすための構造)
//!
//! - 空 → 地面の縦方向グラデーション(地平線に強い水平エッジ)
//! - 軸平行の「ビル」矩形 4 棟 + 窓格子(水平・垂直の支配的な直線群)
//!   → atx-geometry の傾き検出が H/V 両族を拾い、ほぼ 0° を返す
//! - 街灯のポール、地面のタイル目地(細かい直線)
//! - LCG 由来の微細ノイズと粒状テクスチャ(JPEG が平坦画像にならないように)
//! - RGB 各チャンネルに散らばる色(チャンネル取り違えを検出できるよう非対称にする)
//!
//! メタデータは APP2 ICC(ダミー 256 バイト)のみ。EXIF は付けない
//! (フィクスチャが EXIF レスであることをテストが前提にしている)。

use std::path::PathBuf;

use image::{Rgb, RgbImage};

use atx_core::{Limits, Operation, OutputFormat, RotateCrop, TransformRecipe};
use atx_geometry::{detect_tilt, DetectParams};

/// `tilted_scene.jpg` に加える回転角(度、atx-core の規約で正 = 時計回り)。
/// 水平に戻す補正角はこの符号反転(+2.4°)になる想定。
const TILT_ROTATE_DEGREES: f64 = -2.4;

/// フィクスチャの寸法(実写フィクスチャからの移行時に維持した値)。
const WIDTH: u32 = 1477;
const HEIGHT: u32 = 1108;
/// 地平線の y 座標。
const HORIZON: u32 = 664;
/// JPEG 品質(atx-core の既定と同じ)。
const QUALITY: u8 = 85;

/// 決定論的な擬似乱数(Numerical Recipes の LCG)。
struct Lcg(u32);

impl Lcg {
    fn next_u32(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        self.0
    }

    /// -1.0 ..= 1.0 のノイズ。
    fn noise(&mut self) -> f32 {
        (self.next_u32() >> 8) as f32 / (1 << 23) as f32 - 1.0
    }

    fn byte(&mut self) -> u8 {
        (self.next_u32() >> 24) as u8
    }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        lerp(a[0], b[0], t),
        lerp(a[1], b[1], t),
        lerp(a[2], b[2], t),
    ]
}

fn put(buf: &mut [[f32; 3]], x: u32, y: u32, c: [f32; 3]) {
    if x < WIDTH && y < HEIGHT {
        buf[(y * WIDTH + x) as usize] = c;
    }
}

/// 軸平行の塗り矩形(x, y は左上、範囲外はクリップ)。
fn fill_rect(buf: &mut [[f32; 3]], x0: u32, y0: u32, w: u32, h: u32, c: [f32; 3]) {
    for y in y0..(y0 + h).min(HEIGHT) {
        for x in x0..(x0 + w).min(WIDTH) {
            put(buf, x, y, c);
        }
    }
}

/// 建物 1 棟: 面ごとに明度差のあるファサード + 窓格子 + 屋上のパラペット。
///
/// 窓格子が水平・垂直エッジを大量に供給するので、傾き検出の H/V 両族が立つ。
#[allow(clippy::too_many_arguments)]
fn building(
    buf: &mut [[f32; 3]],
    x0: u32,
    top: u32,
    w: u32,
    base: [f32; 3],
    win: [f32; 3],
    win_w: u32,
    win_h: u32,
    rng: &mut Lcg,
) {
    let h = HORIZON.saturating_sub(top) + 24;

    // ファサード: 上ほどわずかに明るい(空からの照り返し)縦グラデーション。
    for y in top..(top + h).min(HEIGHT) {
        let t = (y - top) as f32 / h.max(1) as f32;
        let c = mix(
            [base[0] * 1.08, base[1] * 1.08, base[2] * 1.10],
            [base[0] * 0.82, base[1] * 0.82, base[2] * 0.86],
            t,
        );
        for x in x0..(x0 + w).min(WIDTH) {
            put(buf, x, y, c);
        }
    }

    // 屋上パラペット(強い水平エッジ)。
    fill_rect(
        buf,
        x0,
        top,
        w,
        9,
        [base[0] * 0.55, base[1] * 0.55, base[2] * 0.62],
    );
    // 両端の縁(強い垂直エッジ)。
    fill_rect(
        buf,
        x0,
        top,
        3,
        h,
        [base[0] * 0.60, base[1] * 0.60, base[2] * 0.66],
    );
    fill_rect(
        buf,
        x0 + w.saturating_sub(3),
        top,
        3,
        h,
        [base[0] * 0.48, base[1] * 0.48, base[2] * 0.54],
    );

    // 窓格子。列/行のピッチは窓寸法 + 目地。
    let px = win_w + 14;
    let py = win_h + 16;
    let mut wy = top + 26;
    while wy + win_h < HORIZON + 8 {
        let mut wx = x0 + 16;
        while wx + win_w + 16 <= x0 + w {
            // 窓ごとに明るさを変える(点灯/消灯のばらつき)。
            let k = 0.55 + 0.45 * (rng.next_u32() >> 24) as f32 / 255.0;
            fill_rect(
                buf,
                wx,
                wy,
                win_w,
                win_h,
                [win[0] * k, win[1] * k, win[2] * k],
            );
            // 窓枠の下端(水平エッジを増やす)。
            fill_rect(buf, wx, wy + win_h, win_w, 2, [30.0, 28.0, 26.0]);
            wx += px;
        }
        wy += py;
    }
}

/// シーンを浮動小数バッファに描く。
fn draw_scene() -> Vec<[f32; 3]> {
    let mut buf = vec![[0f32; 3]; (WIDTH * HEIGHT) as usize];
    let mut rng = Lcg(0x5EED_1234);

    // --- 空: 上から下へ濃青 → 淡い黄味へ。
    let sky_top = [46.0, 92.0, 168.0];
    let sky_bottom = [206.0, 214.0, 196.0];
    let ground_top = [126.0, 116.0, 96.0];
    let ground_bottom = [58.0, 50.0, 44.0];
    for y in 0..HEIGHT {
        let c = if y < HORIZON {
            let t = y as f32 / HORIZON as f32;
            mix(sky_top, sky_bottom, t * t)
        } else {
            let t = (y - HORIZON) as f32 / (HEIGHT - HORIZON) as f32;
            mix(ground_top, ground_bottom, t.sqrt())
        };
        for x in 0..WIDTH {
            put(&mut buf, x, y, c);
        }
    }

    // --- 雲(横に伸びた柔らかい帯。垂直方向のエッジは作らない)。
    for band in 0..5u32 {
        let cy = 60.0 + band as f32 * 84.0;
        let amp = 16.0 + band as f32 * 5.0;
        for y in 0..HORIZON {
            let d = (y as f32 - cy).abs();
            if d > amp {
                continue;
            }
            let k = (1.0 - d / amp) * 0.35;
            for x in 0..WIDTH {
                let wobble = ((x as f32 * 0.011 + band as f32).sin() * 0.5 + 0.5) * k;
                let p = &mut buf[(y * WIDTH + x) as usize];
                *p = mix(*p, [244.0, 246.0, 250.0], wobble);
            }
        }
    }

    // --- 遠景の建物(低コントラスト・かすみ)。
    for (i, x0) in [40u32, 300, 620, 980, 1290].into_iter().enumerate() {
        let top = HORIZON - 120 - (i as u32 % 3) * 40;
        let w = 150 + (i as u32 % 4) * 40;
        let c = [150.0 + i as f32 * 6.0, 152.0, 158.0 - i as f32 * 4.0];
        fill_rect(&mut buf, x0, top, w.min(WIDTH - x0), HORIZON - top, c);
    }

    // --- 主要な建物 4 棟(強い水平・垂直エッジ源)。
    building(
        &mut buf,
        70,
        188,
        330,
        [176.0, 92.0, 74.0],
        [236.0, 214.0, 150.0],
        34,
        44,
        &mut rng,
    );
    building(
        &mut buf,
        450,
        96,
        280,
        [78.0, 108.0, 122.0],
        [214.0, 232.0, 240.0],
        28,
        38,
        &mut rng,
    );
    building(
        &mut buf,
        780,
        250,
        360,
        [196.0, 176.0, 138.0],
        [96.0, 120.0, 148.0],
        40,
        30,
        &mut rng,
    );
    building(
        &mut buf,
        1190,
        150,
        250,
        [110.0, 128.0, 84.0],
        [240.0, 226.0, 196.0],
        30,
        46,
        &mut rng,
    );

    // --- 街灯のポール(細い垂直線)とその横木。
    for x0 in [418u32, 762, 1160] {
        fill_rect(&mut buf, x0, HORIZON - 300, 6, 320, [42.0, 40.0, 44.0]);
        fill_rect(&mut buf, x0 - 28, HORIZON - 300, 62, 7, [52.0, 48.0, 50.0]);
    }

    // --- 地面のタイル目地(水平線 + 垂直線)。
    let mut y = HORIZON + 26;
    let mut step = 14u32;
    while y < HEIGHT {
        fill_rect(&mut buf, 0, y, WIDTH, 3, [40.0, 36.0, 32.0]);
        step += 4;
        y += step;
    }
    for x in (0..WIDTH).step_by(96) {
        for yy in HORIZON..HEIGHT {
            let p = &mut buf[(yy * WIDTH + x) as usize];
            *p = mix(*p, [46.0, 42.0, 38.0], 0.5);
        }
    }
    // 地平線そのものを縁取る(最も強い水平エッジ)。
    fill_rect(&mut buf, 0, HORIZON, WIDTH, 4, [64.0, 56.0, 48.0]);

    // --- 微細テクスチャ: 粒状ノイズ + 低周波のムラ。
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let p = &mut buf[(y * WIDTH + x) as usize];
            let n = rng.noise();
            let grain = [n * 7.0, n * 6.0, n * 8.0];
            let vign = 1.0
                - 0.12
                    * (((x as f32 / WIDTH as f32) - 0.5).powi(2)
                        + ((y as f32 / HEIGHT as f32) - 0.5).powi(2));
            for c in 0..3 {
                p[c] = (p[c] + grain[c]) * vign;
            }
        }
    }

    buf
}

fn to_rgb(buf: &[[f32; 3]]) -> RgbImage {
    let mut img = RgbImage::new(WIDTH, HEIGHT);
    for (i, px) in buf.iter().enumerate() {
        let x = (i as u32) % WIDTH;
        let y = (i as u32) / WIDTH;
        img.put_pixel(
            x,
            y,
            Rgb([
                px[0].round().clamp(0.0, 255.0) as u8,
                px[1].round().clamp(0.0, 255.0) as u8,
                px[2].round().clamp(0.0, 255.0) as u8,
            ]),
        );
    }
    img
}

/// APP2 に埋め込むダミー ICC ペイロード(256 バイト、固定シードの LCG 由来)。
///
/// atx-core は ICC を「検出してコピーする」だけで中身を解釈しないため、
/// プロファイルとして妥当である必要はない。
fn dummy_icc() -> Vec<u8> {
    let mut rng = Lcg(0x1CC0_0001);
    (0..256).map(|_| rng.byte()).collect()
}

/// atx-core の `codec::encode_jpeg` と同じ設定
/// (ベースライン / 単一インターリーブスキャン / 最適化ハフマンなし)。
fn encode_jpeg(img: &RgbImage, icc: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = jpeg_encoder::Encoder::new(&mut out, QUALITY);
    encoder.set_progressive(false);
    encoder.set_optimized_huffman_tables(false);
    encoder
        .add_icc_profile(icc)
        .expect("dummy icc payload must fit in one APP2 segment");
    encoder
        .encode(
            img.as_raw(),
            WIDTH as u16,
            HEIGHT as u16,
            jpeg_encoder::ColorType::Rgb,
        )
        .expect("jpeg encode");
    out
}

fn main() {
    let img = to_rgb(&draw_scene());
    let icc = dummy_icc();

    // 決定論の自己検証: 生成 → エンコードを 2 回通してバイト同一を要求する。
    let first = encode_jpeg(&img, &icc);
    let second = encode_jpeg(&to_rgb(&draw_scene()), &dummy_icc());
    assert_eq!(
        first, second,
        "fixture generation must be byte-for-byte deterministic"
    );

    assert!(
        !first.windows(6).any(|w| w == b"Exif\x00\x00"),
        "the fixture must never carry an EXIF segment"
    );
    assert!(
        first.windows(12).any(|w| w == b"ICC_PROFILE\0"),
        "the fixture must carry an APP2 ICC segment"
    );

    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/synthetic_scene.jpg");
    std::fs::create_dir_all(path.parent().unwrap()).expect("fixtures dir");
    std::fs::write(&path, &first).expect("write fixture");
    println!(
        "wrote {} ({}x{}, {} bytes)",
        path.display(),
        WIDTH,
        HEIGHT,
        first.len()
    );

    gen_tilted_fixture(&first);

    // ドキュメント前処理(DESIGN §9.12)のフィクスチャ。
    // 既存の synthetic_scene.jpg / tilted_scene.jpg のバイト列には一切触れない。
    gen_synthetic_document();
    gen_document_photo();
    gen_dark_ui_screenshot();
}

/// `evals/fixtures/tilted_scene.jpg` を生成する。
///
/// `synthetic_scene.jpg` のバイト列(`base_jpeg`)に対し、atx-core の決定論エンジン
/// (`apply_recipe`)自身で回転 + 最大内接矩形クロップを適用し、その出力をそのまま
/// 書き出す。生成の決定論(2 回適用してバイト同一)と、傾き検出の符号・精度
/// (`detect_tilt` が ~+2.4° を十分な confidence で返すこと)の両方をここで検証する。
fn gen_tilted_fixture(base_jpeg: &[u8]) {
    let recipe = tilt_recipe();
    let limits = Limits::default();

    let first = atx_core::apply_recipe(base_jpeg, &recipe, &limits)
        .expect("apply_recipe(rotate) on synthetic scene");
    let second = atx_core::apply_recipe(base_jpeg, &recipe, &limits)
        .expect("apply_recipe(rotate) on synthetic scene (2nd run)");
    assert_eq!(
        first.bytes, second.bytes,
        "tilted fixture generation must be byte-for-byte deterministic"
    );

    // 符号・精度の自己検証: 生成した画像を detect_tilt にかけ、
    // 「時計回りに +2.4° 回すと水平になる」という想定を裏付ける。
    let decoded = image::load_from_memory(&first.bytes).expect("decode generated tilted jpeg");
    let detection = detect_tilt(&decoded, &DetectParams::default());
    let recommended = detection
        .recommended_angle_degrees
        .expect("detect_tilt should recommend a correction angle for the tilted fixture");
    println!(
        "tilted_scene.jpg: detect_tilt recommended_angle_degrees={recommended:.3} confidence={:.3} method={}",
        detection.confidence, detection.method
    );
    assert!(
        (recommended - -TILT_ROTATE_DEGREES).abs() <= 0.3,
        "expected detect_tilt to recommend ~{:.1}° to correct the {:.1}° tilt, got {recommended:.3}° ({detection:?})",
        -TILT_ROTATE_DEGREES,
        TILT_ROTATE_DEGREES
    );
    assert!(
        detection.confidence >= 0.5,
        "expected decent confidence for the tilted fixture, got {} ({detection:?})",
        detection.confidence
    );

    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../evals/fixtures/tilted_scene.jpg");
    std::fs::create_dir_all(path.parent().unwrap()).expect("evals/fixtures dir");
    std::fs::write(&path, &first.bytes).expect("write tilted fixture");
    println!(
        "wrote {} ({}x{}, {} bytes, rotate={}°)",
        path.display(),
        first.width,
        first.height,
        first.bytes.len(),
        TILT_ROTATE_DEGREES
    );
}

fn tilt_recipe() -> TransformRecipe {
    TransformRecipe {
        layers: None,
        operations: vec![
            Operation::Rotate {
                angle_degrees: TILT_ROTATE_DEGREES,
                crop: RotateCrop::LargestInscribedRect,
            },
            Operation::Encode {
                format: OutputFormat::Jpeg,
                quality: Some(QUALITY),
                bit_depth: None,
            },
        ],
    }
}

// ===========================================================================
// ドキュメント前処理(DESIGN §9.12)のフィクスチャ
// ===========================================================================
//
// - tests/fixtures/synthetic_document.png … 白い用紙 + 黒い単語状バー列 + 一様な灰色余白
// - evals/fixtures/document_photo.jpg     … 上の用紙を暗い机面に置き、atx-core の
//                                            `perspective`(キーストーン)で歪ませた「写真」
// - evals/fixtures/dark_ui_screenshot.png … 暗い UI のスクリーンショット風合成
//
// 第三者素材は使わない。**文字は一切描かず**、「文字らしい」矩形バー列で行を表す。
// 乱数源は固定シードの LCG のみ。いずれも 2 回生成してバイト同一を assert する。

/// 任意寸法の描画バッファ(既存のシーン描画はモジュール定数 `WIDTH`/`HEIGHT` に
/// 縛られているため、新しいフィクスチャ用に寸法を持ち回せる最小の器を用意する)。
struct Canvas {
    w: u32,
    h: u32,
    buf: Vec<[f32; 3]>,
}

impl Canvas {
    fn new(w: u32, h: u32, fill: [f32; 3]) -> Self {
        Self {
            w,
            h,
            buf: vec![fill; (w * h) as usize],
        }
    }

    /// 軸平行の塗り矩形(範囲外はクリップ)。
    fn rect(&mut self, x0: i64, y0: i64, w: i64, h: i64, c: [f32; 3]) {
        let x1 = (x0 + w).clamp(0, self.w as i64);
        let y1 = (y0 + h).clamp(0, self.h as i64);
        let x0 = x0.clamp(0, self.w as i64);
        let y0 = y0.clamp(0, self.h as i64);
        for y in y0..y1 {
            for x in x0..x1 {
                self.buf[(y as u32 * self.w + x as u32) as usize] = c;
            }
        }
    }

    fn to_rgb(&self) -> RgbImage {
        let mut img = RgbImage::new(self.w, self.h);
        for (i, px) in self.buf.iter().enumerate() {
            img.put_pixel(
                (i as u32) % self.w,
                (i as u32) / self.w,
                Rgb([
                    px[0].round().clamp(0.0, 255.0) as u8,
                    px[1].round().clamp(0.0, 255.0) as u8,
                    px[2].round().clamp(0.0, 255.0) as u8,
                ]),
            );
        }
        img
    }
}

/// 用紙の色。
const PAPER: [f32; 3] = [246.0, 245.0, 242.0];
/// インク(単語バー)の色。
const INK: [f32; 3] = [28.0, 27.0, 30.0];
/// `synthetic_document.png` の一様な余白色(`trim` の期待値がこれで定義できる)。
const DOC_MARGIN: [f32; 3] = [154.0, 154.0, 154.0];

/// 用紙の中身(見出し + 単語状バーの行)を矩形だけで描く。
///
/// 文字は描かない(フォント = 環境依存 = 非決定論。DESIGN §9.9 と同じ理由)。
/// 行間・左マージンは一定で、単語の幅だけ LCG で振る。
fn draw_document_body(canvas: &mut Canvas, x: i64, y: i64, w: i64, h: i64, rng: &mut Lcg) {
    canvas.rect(x, y, w, h, PAPER);

    let pad = w / 10;
    let text_w = w - pad * 2;
    // 行の高さ・行間は用紙高さに対する固定比(寸法を変えても見た目が保たれる)。
    let bar_h = (h / 62).max(3);
    let line_step = bar_h * 3;
    let space = (bar_h * 3 / 4).max(2);

    // 見出し(太いバー)+ その下の罫線。
    canvas.rect(x + pad, y + line_step, text_w * 3 / 5, bar_h * 2, INK);
    canvas.rect(x + pad, y + line_step * 2, text_w, (bar_h / 3).max(1), INK);

    // 本文: 段落ごとに行を並べ、各行を「単語」バーで埋める。
    let mut ly = y + line_step * 4;
    let mut paragraph_line = 0i64;
    while ly + bar_h < y + h - pad {
        // 段落末尾の行は短くする(自然な右端の凹凸を作る)。
        let line_w = if paragraph_line == 6 {
            text_w * (2 + (rng.next_u32() % 3) as i64) / 5
        } else {
            text_w
        };
        let mut lx = x + pad;
        while lx < x + pad + line_w {
            let word = bar_h * (4 + (rng.next_u32() % 9) as i64) / 2;
            let word = word.min(x + pad + line_w - lx);
            if word <= 0 {
                break;
            }
            canvas.rect(lx, ly, word, bar_h, INK);
            lx += word + space;
        }
        ly += line_step;
        paragraph_line += 1;
        if paragraph_line > 6 {
            paragraph_line = 0;
            ly += line_step; // 段落間の空き
        }
    }
}

/// PNG へ決定論的にエンコードする(image クレートの既定圧縮設定。
/// 同一バージョン・同一入力なら常に同じバイト列)。
fn encode_png(img: &RgbImage) -> Vec<u8> {
    use image::ImageEncoder;
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(
            img.as_raw(),
            img.width(),
            img.height(),
            image::ExtendedColorType::Rgb8,
        )
        .expect("png encode");
    out
}

/// リポジトリルート相対のフィクスチャ出力先(親ディレクトリは作る)。
fn repo_path(relative: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).expect("fixture directory");
    path
}

/// `tests/fixtures/synthetic_document.png`(900x1200、軸平行)。
fn draw_synthetic_document() -> Canvas {
    const W: u32 = 900;
    const H: u32 = 1200;
    /// 用紙の周囲に残す一様な余白(px)。`trim` の期待値はこれで決まる。
    const MARGIN: i64 = 60;

    let mut canvas = Canvas::new(W, H, DOC_MARGIN);
    let mut rng = Lcg(0xD0C0_1234);
    draw_document_body(
        &mut canvas,
        MARGIN,
        MARGIN,
        W as i64 - MARGIN * 2,
        H as i64 - MARGIN * 2,
        &mut rng,
    );
    canvas
}

fn gen_synthetic_document() {
    let first = encode_png(&draw_synthetic_document().to_rgb());
    let second = encode_png(&draw_synthetic_document().to_rgb());
    assert_eq!(
        first, second,
        "synthetic_document.png generation must be byte-for-byte deterministic"
    );
    let path = repo_path("tests/fixtures/synthetic_document.png");
    std::fs::write(&path, &first).expect("write synthetic_document.png");
    println!("wrote {} (900x1200, {} bytes)", path.display(), first.len());
}

/// `document_photo.jpg` のシーン寸法。
const PHOTO_W: u32 = 1400;
const PHOTO_H: u32 = 1050;
/// 机の上に置いた用紙(軸平行、キーストーンを掛ける前)の矩形。
const PAPER_X: i64 = 300;
const PAPER_Y: i64 = 90;
const PAPER_W: i64 = 800;
const PAPER_H: i64 = 920;
/// 掛けるキーストーン角(度)。atx-core `perspective` の符号規約。
const PHOTO_VERTICAL_DEG: f64 = 9.0;
const PHOTO_HORIZONTAL_DEG: f64 = 5.0;
/// 机の色(キーストーンで空いた領域の pad_color にも使う)。
const DESK_HEX: &str = "#1e1c1a";

/// キーストーン前のシーン(暗い机 + 用紙 + 微細ノイズ)。
fn draw_document_scene() -> Canvas {
    let mut canvas = Canvas::new(PHOTO_W, PHOTO_H, [30.0, 28.0, 26.0]);
    let mut rng = Lcg(0x0DE5_C001);

    // 机: ゆるい縦グラデーション + 木目風の低周波の筋(用紙より必ず暗い)。
    for y in 0..PHOTO_H {
        let t = y as f32 / PHOTO_H as f32;
        let base = [
            lerp(38.0, 22.0, t),
            lerp(34.0, 20.0, t),
            lerp(30.0, 18.0, t),
        ];
        for x in 0..PHOTO_W {
            let grain = ((x as f32 * 0.02).sin() * 0.5 + 0.5) * 3.0;
            canvas.buf[(y * PHOTO_W + x) as usize] =
                [base[0] + grain, base[1] + grain, base[2] + grain * 0.8];
        }
    }

    draw_document_body(&mut canvas, PAPER_X, PAPER_Y, PAPER_W, PAPER_H, &mut rng);

    // 用紙のわずかな影(下辺・右辺)。平坦すぎる合成画像で JPEG が
    // ブロックノイズだらけにならないようにするための味付け。
    for i in 0..6i64 {
        let k = 1.0 - i as f32 / 6.0;
        let c = [26.0 * k + 20.0, 24.0 * k + 18.0, 22.0 * k + 16.0];
        canvas.rect(PAPER_X + 6, PAPER_Y + PAPER_H + i, PAPER_W, 1, c);
        canvas.rect(PAPER_X + PAPER_W + i, PAPER_Y + 6, 1, PAPER_H, c);
    }

    // 微細ノイズ(全面)。
    for px in canvas.buf.iter_mut() {
        let n = rng.noise() * 3.0;
        for c in px.iter_mut() {
            *c += n;
        }
    }
    canvas
}

/// キーストーンの前進写像(atx-core `ops::perspective` の doc コメントのモデル)。
///
/// 画像中心を原点とする連続座標で `X' = X / D`, `Y' = Y / D`,
/// `D = 1 + (t_h X + t_v Y) / f`、`f = max(W, H)`。
/// 生成器はこの式で「既知 quad」を解析的に求め、`detect_document` の答え合わせに使う。
/// (エンジン側は正規化座標で 1e-6 量子化して行列を組むので厳密には一致しないが、
///  差は 0.01 画素未満で、判定に使う長辺 1% の許容とは 3 桁違う)
fn keystone_forward(p: [f64; 2]) -> [f64; 2] {
    let f = PHOTO_W.max(PHOTO_H) as f64;
    let (cx, cy) = (PHOTO_W as f64 / 2.0, PHOTO_H as f64 / 2.0);
    let t_v = PHOTO_VERTICAL_DEG.to_radians().tan();
    let t_h = PHOTO_HORIZONTAL_DEG.to_radians().tan();
    let (x, y) = (p[0] - cx, p[1] - cy);
    let d = 1.0 + (t_h * x + t_v * y) / f;
    [x / d + cx, y / d + cy]
}

/// 任意寸法版の JPEG エンコード(ICC なし)。既存の [`encode_jpeg`] は
/// `synthetic_scene.jpg` 専用(モジュール定数の寸法 + ダミー ICC)なので分けてある。
fn encode_jpeg_sized(img: &RgbImage, quality: u8) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = jpeg_encoder::Encoder::new(&mut out, quality);
    encoder.set_progressive(false);
    encoder.set_optimized_huffman_tables(false);
    encoder
        .encode(
            img.as_raw(),
            img.width() as u16,
            img.height() as u16,
            jpeg_encoder::ColorType::Rgb,
        )
        .expect("jpeg encode");
    out
}

/// `evals/fixtures/document_photo.jpg` を生成する。
///
/// `tilted_scene.jpg` と同じ「生成器が自分の検出器で答え合わせする」方針:
/// 既知の用紙矩形をキーストーンで写した quad を解析的に求め、生成直後に
/// `atx_geometry::detect_document` を走らせて頂点誤差 ≤ 長辺 1% を assert する。
fn gen_document_photo() {
    let scene = draw_document_scene().to_rgb();
    let scene_jpeg = encode_jpeg_sized(&scene, QUALITY);
    let recipe = TransformRecipe {
        layers: None,
        operations: vec![
            Operation::Perspective {
                quad: None,
                vertical_degrees: Some(PHOTO_VERTICAL_DEG),
                horizontal_degrees: Some(PHOTO_HORIZONTAL_DEG),
                pad_color: Some(DESK_HEX.to_string()),
            },
            Operation::Encode {
                format: OutputFormat::Jpeg,
                quality: Some(QUALITY),
                bit_depth: None,
            },
        ],
    };
    let limits = Limits::default();
    let first = atx_core::apply_recipe(&scene_jpeg, &recipe, &limits)
        .expect("apply_recipe(perspective) on the document scene");
    let second = atx_core::apply_recipe(&scene_jpeg, &recipe, &limits)
        .expect("apply_recipe(perspective) on the document scene (2nd run)");
    assert_eq!(
        first.bytes, second.bytes,
        "document_photo.jpg generation must be byte-for-byte deterministic"
    );

    // 既知 quad(tl, tr, br, bl)。
    let expected = [
        keystone_forward([PAPER_X as f64, PAPER_Y as f64]),
        keystone_forward([(PAPER_X + PAPER_W) as f64, PAPER_Y as f64]),
        keystone_forward([(PAPER_X + PAPER_W) as f64, (PAPER_Y + PAPER_H) as f64]),
        keystone_forward([PAPER_X as f64, (PAPER_Y + PAPER_H) as f64]),
    ];

    let decoded = image::load_from_memory(&first.bytes).expect("decode generated document photo");
    let detection =
        atx_geometry::detect_document(&decoded, &atx_geometry::DocumentParams::default());
    let quad = detection
        .quad
        .unwrap_or_else(|| panic!("detect_document must find the sheet of paper ({detection:?})"));
    let tolerance = PHOTO_W.max(PHOTO_H) as f64 * 0.01;
    let mut worst = 0f64;
    for (i, (got, want)) in quad.iter().zip(expected.iter()).enumerate() {
        let d = ((got[0] - want[0]).powi(2) + (got[1] - want[1]).powi(2)).sqrt();
        worst = worst.max(d);
        assert!(
            d <= tolerance,
            "corner {i} is {d:.2}px away from the known quad (tolerance {tolerance:.2}px); \
             got {got:?}, expected {want:?} ({detection:?})"
        );
    }
    println!(
        "document_photo.jpg: detect_document confidence={:.3} area_ratio={:.3} worst corner error={worst:.2}px (tolerance {tolerance:.1}px) hint={:?}",
        detection.confidence, detection.area_ratio, detection.output_size_hint
    );

    let path = repo_path("evals/fixtures/document_photo.jpg");
    std::fs::write(&path, &first.bytes).expect("write document_photo.jpg");
    println!(
        "wrote {} ({}x{}, {} bytes)",
        path.display(),
        first.width,
        first.height,
        first.bytes.len()
    );
}

/// `evals/fixtures/dark_ui_screenshot.png`(1280x800)。
///
/// 暗い背景に**一様な余白**(= `trim` の期待値が定義できる)と、明るい UI 要素風の
/// 矩形群。文字は描かず、ラベルは短いバーで表す。
fn draw_dark_ui() -> Canvas {
    const W: u32 = 1280;
    const H: u32 = 800;
    /// 左右の余白(この外側は完全に一様な背景色)。
    const MX: i64 = 110;
    /// 上下の余白。
    const MY: i64 = 80;

    let bg = [16.0, 16.0, 20.0];
    let mut canvas = Canvas::new(W, H, bg);
    let mut rng = Lcg(0xDA2C_0FFE);

    let (x0, y0) = (MX, MY);
    let (cw, ch) = (W as i64 - MX * 2, H as i64 - MY * 2);

    // ウィンドウ本体(背景よりわずかに明るい面)。
    canvas.rect(x0, y0, cw, ch, [30.0, 31.0, 38.0]);
    // タイトルバー + 信号機ボタン風の点。
    canvas.rect(x0, y0, cw, 34, [44.0, 46.0, 56.0]);
    for i in 0..3i64 {
        canvas.rect(x0 + 14 + i * 20, y0 + 12, 10, 10, [96.0, 100.0, 120.0]);
    }
    // サイドバー + その項目。
    let side_w = cw / 4;
    canvas.rect(x0, y0 + 34, side_w, ch - 34, [24.0, 25.0, 31.0]);
    for i in 0..8i64 {
        let iy = y0 + 60 + i * 40;
        canvas.rect(x0 + 18, iy, 14, 14, [120.0, 126.0, 150.0]);
        let w = side_w / 2 + (rng.next_u32() % 60) as i64;
        canvas.rect(
            x0 + 42,
            iy + 3,
            w.min(side_w - 60),
            9,
            [206.0, 208.0, 220.0],
        );
    }
    // 本文側のカード群(明るい見出しバー + 細い本文バー)。
    let main_x = x0 + side_w + 24;
    let main_w = cw - side_w - 48;
    for card in 0..3i64 {
        let cy = y0 + 60 + card * 190;
        canvas.rect(main_x, cy, main_w, 160, [40.0, 42.0, 52.0]);
        canvas.rect(main_x + 20, cy + 22, main_w / 3, 16, [232.0, 234.0, 244.0]);
        for line in 0..4i64 {
            let w = main_w - 40 - (rng.next_u32() % 160) as i64;
            canvas.rect(
                main_x + 20,
                cy + 60 + line * 22,
                w.max(60),
                8,
                [150.0, 154.0, 172.0],
            );
        }
        // 強調ボタン(いちばん明るい要素)。
        canvas.rect(
            main_x + main_w - 130,
            cy + 118,
            110,
            26,
            [96.0, 148.0, 236.0],
        );
    }
    canvas
}

fn gen_dark_ui_screenshot() {
    let first = encode_png(&draw_dark_ui().to_rgb());
    let second = encode_png(&draw_dark_ui().to_rgb());
    assert_eq!(
        first, second,
        "dark_ui_screenshot.png generation must be byte-for-byte deterministic"
    );
    let path = repo_path("evals/fixtures/dark_ui_screenshot.png");
    std::fs::write(&path, &first).expect("write dark_ui_screenshot.png");
    println!("wrote {} (1280x800, {} bytes)", path.display(), first.len());
}
