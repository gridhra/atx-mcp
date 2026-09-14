//! 知覚ハッシュ(dHash)・SSIM・EXIF 全量取得のテスト。
//!
//! # ゴールデン値について
//!
//! `GOLDEN_*` の定数は「CI の macOS arm64 と Linux x86_64 の両方で同じ値が出ること」を
//! 固定するためのもので、`tests/f32_spike.rs` と同じ位置づけ(ローカル 1 アームの green
//! だけではクロスプラットフォーム決定論の証明にならない)。
//!
//! dHash は整数演算だけで作るので浮動小数の実装差が入る余地がない。SSIM は窓ごとの
//! 割り算に f64 を使うが、`mul_add` 禁止・走査順の左結合・1e-4 量子化で差を潰している。

use atx_core::similarity::{
    dhash_hex, dhash_rgb8, dhash_rgba8, gray_from_rgb8, gray_from_rgba8, hamming, ssim_gray,
};
use atx_core::{apply_recipe, inspect_bytes, inspect_bytes_with, read_exif_all, Limits};
use image::{GrayImage, Luma, RgbaImage};

const SCENE: &[u8] = include_bytes!("../../../tests/fixtures/synthetic_scene.jpg");
const DOCUMENT: &[u8] = include_bytes!("../../../tests/fixtures/synthetic_document.png");

/// 合成フィクスチャ 2 枚の dHash(両 CI アームで一致すること)。
const GOLDEN_SCENE_DHASH: &str = "22e7ada66da08040";
const GOLDEN_DOCUMENT_DHASH: &str = "8ca68e96868a8e80";
/// scene と「scene を blur σ2 したもの」の SSIM(1e-4 量子化済み)。
const GOLDEN_SCENE_VS_BLUR_SSIM: f64 = 0.771;

fn decode_rgba(bytes: &[u8]) -> RgbaImage {
    image::load_from_memory(bytes)
        .expect("fixture decodes")
        .to_rgba8()
}

fn hash_of(bytes: &[u8]) -> u64 {
    let img = decode_rgba(bytes);
    dhash_rgba8(img.as_raw(), img.width(), img.height())
}

fn gray_of(bytes: &[u8]) -> GrayImage {
    let img = decode_rgba(bytes);
    gray_from_rgba8(img.as_raw(), img.width(), img.height()).expect("gray conversion")
}

fn apply(bytes: &[u8], json: &str) -> Vec<u8> {
    let recipe = serde_json::from_str(json).expect("recipe parses");
    apply_recipe(bytes, &recipe, &Limits::default())
        .expect("apply_recipe succeeds")
        .bytes
}

// ---------------------------------------------------------------------------
// SSIM
// ---------------------------------------------------------------------------

/// 自分自身との SSIM は厳密に 1.0。
///
/// 偶然ではなく構造上そうなる: a == b なら窓ごとの分子と分母が同じ式・同じ順序で
/// 作られるため、割り算の結果がビット単位で 1.0 になる。
#[test]
fn ssim_of_identical_image_is_one() {
    let gray = gray_of(SCENE);
    assert_eq!(ssim_gray(&gray, &gray), Some(1.0));
}

/// 階調を反転した画像との SSIM は大きく落ちる(構造は同じでも輝度・共分散が反転するため)。
#[test]
fn ssim_of_inverted_image_is_low() {
    let gray = gray_of(SCENE);
    let inverted = GrayImage::from_fn(gray.width(), gray.height(), |x, y| {
        Luma([255 - gray.get_pixel(x, y)[0]])
    });
    let s = ssim_gray(&gray, &inverted).expect("same dimensions");
    assert!(s < 0.3, "反転画像の SSIM は 0.3 未満: {s}");
}

/// 寸法が違う場合は「低い類似度」ではなく「測れない」= None。
#[test]
fn ssim_requires_matching_dimensions() {
    let a = gray_of(SCENE);
    let b = gray_of(DOCUMENT);
    assert_ne!(a.dimensions(), b.dimensions());
    assert_eq!(ssim_gray(&a, &b), None);
}

/// 同一入力 → ビット単位で同一の SSIM。
#[test]
fn ssim_is_deterministic() {
    let a = gray_of(SCENE);
    let b = gray_of(&apply(SCENE, r#"{"operations":[{"op":"blur","sigma":2}]}"#));
    let first = ssim_gray(&a, &b).unwrap();
    let second = ssim_gray(&a, &b).unwrap();
    assert_eq!(first.to_bits(), second.to_bits());
}

/// ぼかした画像との SSIM: 0 と 1 の間に落ち、値をゴールデンとして固定する。
#[test]
fn ssim_against_blurred_matches_golden() {
    let a = gray_of(SCENE);
    let b = gray_of(&apply(SCENE, r#"{"operations":[{"op":"blur","sigma":2}]}"#));
    let s = ssim_gray(&a, &b).expect("same dimensions");
    assert!((0.0..1.0).contains(&s), "ぼかし後の SSIM は 0..1: {s}");
    assert_eq!(
        s, GOLDEN_SCENE_VS_BLUR_SSIM,
        "SSIM ゴールデンが動いた(両 CI アームで一致する値であること)"
    );
}

// ---------------------------------------------------------------------------
// dHash
// ---------------------------------------------------------------------------

/// 半分に縮小しても同じ絵と見なせる(ハミング距離 8 以下)。
///
/// dHash は 9x8 セルの面積平均の大小関係しか見ないので、解像度が変わっても
/// 大域的な明暗のパターンが保たれていれば距離は小さいままになる。
#[test]
fn dhash_survives_downscaling() {
    let original = hash_of(SCENE);
    let half = hash_of(&apply(
        SCENE,
        r#"{"operations":[{"op":"resize","width":738,"fit":"contain"}]}"#,
    ));
    let d = hamming(original, half);
    assert!(d <= 8, "半分に縮小した画像との距離は 8 以下: {d}");
}

/// 別の絵(写真と書類)は遠い。
#[test]
fn dhash_separates_different_pictures() {
    let d = hamming(hash_of(SCENE), hash_of(DOCUMENT));
    assert!(d >= 20, "別画像どうしの距離は 20 以上: {d}");
}

/// フィクスチャの dHash をゴールデンとして固定する(両 CI アームで一致すること)。
#[test]
fn dhash_matches_golden() {
    assert_eq!(dhash_hex(hash_of(SCENE)), GOLDEN_SCENE_DHASH);
    assert_eq!(dhash_hex(hash_of(DOCUMENT)), GOLDEN_DOCUMENT_DHASH);
}

/// 同一入力 → 同一ハッシュ。RGB8 経路と RGBA8 経路も一致する。
#[test]
fn dhash_is_deterministic() {
    assert_eq!(hash_of(SCENE), hash_of(SCENE));

    let rgb = image::load_from_memory(SCENE).unwrap().to_rgb8();
    let rgba = decode_rgba(SCENE);
    assert_eq!(
        dhash_rgb8(rgb.as_raw(), rgb.width(), rgb.height()),
        dhash_rgba8(rgba.as_raw(), rgba.width(), rgba.height())
    );
}

/// EXIF Orientation を持つ原本と、その「encode だけ」の派生の dHash は近い。
///
/// `apply_recipe` は orientation を画素へ焼き込むので、`inspect_bytes` が
/// 正規化**前**の画素でハッシュを取ると、同じ絵なのに距離が 30 近くまで開いていた
/// (90 度回った絵は別の絵として扱われる)。ハッシュは orientation 適用後に取る。
#[test]
fn dhash_is_computed_after_exif_orientation() {
    let rotated = jpeg_with_exif(SCENE, &orientation_only_tiff(6));

    let info = inspect_bytes(&rotated, &Limits::default()).unwrap();
    assert_eq!(info.exif_orientation, Some(6));
    let original = hash_hex_to_u64(info.perceptual_hash.as_deref().unwrap());

    // encode だけの派生(画素は orientation を焼いたもの、EXIF は落ちる)。
    let derived = apply(
        &rotated,
        r#"{"operations":[{"op":"encode","format":"png"}]}"#,
    );
    let derived_info = inspect_bytes(&derived, &Limits::default()).unwrap();
    assert_eq!(derived_info.exif_orientation, None);
    let derived_hash = hash_hex_to_u64(derived_info.perceptual_hash.as_deref().unwrap());

    let d = hamming(original, derived_hash);
    assert!(
        d <= 5,
        "orientation を焼いただけの派生との距離は 5 以下であること: {d}"
    );
}

/// EXIF Orientation=1 相当(タグはあるが無変換)では、EXIF なしのハッシュと一致する。
#[test]
fn dhash_with_orientation_one_matches_the_exif_free_hash() {
    let tagged = jpeg_with_exif(SCENE, &orientation_only_tiff(1));
    let info = inspect_bytes(&tagged, &Limits::default()).unwrap();
    assert_eq!(info.perceptual_hash.as_deref(), Some(GOLDEN_SCENE_DHASH));
}

/// RGB8 経路と RGBA8 経路のグレー化は**ビット同一**(輝度はアルファを使わない)。
///
/// `compare_revisions` の SSIM がフル解像度を RGBA8 へ広げずに RGB8 のまま
/// 渡せる根拠はこれ(以前は clone + RGBA8 変換を 2 枚分やっていた)。
#[test]
fn gray_from_rgb8_matches_gray_from_rgba8() {
    let rgb = image::load_from_memory(SCENE).unwrap().to_rgb8();
    let rgba = decode_rgba(SCENE);
    let from_rgb = gray_from_rgb8(rgb.as_raw(), rgb.width(), rgb.height()).expect("gray");
    let from_rgba = gray_from_rgba8(rgba.as_raw(), rgba.width(), rgba.height()).expect("gray");
    assert_eq!(from_rgb.as_raw(), from_rgba.as_raw());
}

fn hash_hex_to_u64(hex: &str) -> u64 {
    u64::from_str_radix(hex, 16).expect("dHash は 16 桁の 16 進")
}

/// Orientation タグ 1 件だけの TIFF ブロック([`jpeg_with_exif`] に渡す)。
fn orientation_only_tiff(orientation: u16) -> Vec<u8> {
    let mut tiff: Vec<u8> = Vec::new();
    tiff.extend_from_slice(b"MM\x00\x2a");
    tiff.extend_from_slice(&8u32.to_be_bytes()); // IFD0 のオフセット
    tiff.extend_from_slice(&1u16.to_be_bytes()); // エントリ数
    entry(&mut tiff, 0x0112, 3, 1, u32::from(orientation) << 16); // Orientation (SHORT)
    tiff.extend_from_slice(&0u32.to_be_bytes()); // 次の IFD なし
    tiff
}

/// `inspect_bytes` は常に perceptual_hash を埋める(単独計算と一致する)。
#[test]
fn inspect_fills_perceptual_hash() {
    let info = inspect_bytes(SCENE, &Limits::default()).unwrap();
    assert_eq!(info.perceptual_hash.as_deref(), Some(GOLDEN_SCENE_DHASH));

    let v = serde_json::to_value(&info).unwrap();
    assert_eq!(v["perceptual_hash"], serde_json::json!(GOLDEN_SCENE_DHASH));
}

// ---------------------------------------------------------------------------
// EXIF 全量取得
// ---------------------------------------------------------------------------

/// フィクスチャは EXIF レス(リポジトリの規律)なので空 Vec。
/// 既定の `inspect_bytes` では `exif` フィールドが JSON に出ないことも固定する。
#[test]
fn exif_free_fixture_yields_no_entries() {
    assert!(read_exif_all(SCENE).is_empty());
    assert!(read_exif_all(DOCUMENT).is_empty());

    let info = inspect_bytes(SCENE, &Limits::default()).unwrap();
    assert!(info.exif.is_none());
    let v = serde_json::to_value(&info).unwrap();
    assert!(v.get("exif").is_none(), "既定出力に exif は出ない");

    let opted_in = inspect_bytes_with(SCENE, &Limits::default(), true).unwrap();
    assert_eq!(opted_in.exif.as_deref(), Some(&[][..]));
}

/// 手組みの最小 EXIF を入れた JPEG から primary / exif / gps の 3 IFD が読めること。
///
/// kamadak-exif の書き込み機能は使わず、APP1 セグメント(TIFF ヘッダ + 3 つの IFD)を
/// バイト列として自分で組む。目的は「read_exif_all が IFD 名・タグ名・値をどう並べるか」を
/// 外部ライブラリの挙動に依存せず固定すること。
#[test]
fn hand_built_exif_is_read_back() {
    let jpeg = jpeg_with_exif(SCENE, &minimal_exif(SHORT_DESCRIPTION));
    let entries = read_exif_all(&jpeg);
    assert!(!entries.is_empty());

    let find = |ifd: &str, tag: &str| {
        entries
            .iter()
            .find(|e| e.ifd == ifd && e.tag == tag)
            .unwrap_or_else(|| panic!("{ifd}/{tag} が見つからない: {entries:?}"))
    };

    // primary IFD: ImageDescription(ポインタ系タグ自体は kamadak が消費する)
    assert!(find("primary", "ImageDescription")
        .value
        .contains(SHORT_DESCRIPTION));
    // exif IFD: DateTimeOriginal
    assert!(find("exif", "DateTimeOriginal")
        .value
        .contains("2026-09-14 12:34:56"));
    // gps IFD: GPSLatitude(度分秒 + 単位付き)
    let lat = &find("gps", "GPSLatitude").value;
    assert!(lat.contains("35"), "GPSLatitude の値: {lat}");

    // 既存の has_gps も true になる(同じ APP1 を両方の経路が読む)。
    let info = inspect_bytes_with(&jpeg, &Limits::default(), true).unwrap();
    assert!(info.has_gps);
    assert_eq!(info.exif.as_ref().map(Vec::len), Some(entries.len()));
}

/// 300 文字の ImageDescription は 256 文字 + `...` に切り詰められる。
#[test]
fn long_values_are_truncated_to_256_chars() {
    let long: String = std::iter::repeat_n('a', 300).collect();
    let jpeg = jpeg_with_exif(SCENE, &minimal_exif(&long));
    let entries = read_exif_all(&jpeg);
    let description = entries
        .iter()
        .find(|e| e.tag == "ImageDescription")
        .expect("ImageDescription");

    assert!(description.value.ends_with("..."));
    // 256 文字 + "..." の 3 文字。引用符が付く形式でも文字数は変わらない。
    assert_eq!(description.value.chars().count(), 259);
    assert!(description.value.starts_with("\"aaa") || description.value.starts_with("aaa"));
}

/// EXIF 全量は 2 回読んで同一(決定論)。
#[test]
fn exif_all_is_deterministic() {
    let jpeg = jpeg_with_exif(SCENE, &minimal_exif(SHORT_DESCRIPTION));
    assert_eq!(read_exif_all(&jpeg), read_exif_all(&jpeg));
}

// ---------------------------------------------------------------------------
// 手組み EXIF ビルダ(テスト専用)
// ---------------------------------------------------------------------------

const SHORT_DESCRIPTION: &str = "atx synthetic test";

/// 最小構成の EXIF(TIFF ブロック)を組む。
///
/// レイアウト(TIFF 先頭からのオフセット):
///
/// | 範囲 | 内容 |
/// |---|---|
/// | 0..8 | TIFF ヘッダ(`MM`, 0x002A, IFD0 = 8) |
/// | 8..50 | IFD0: ImageDescription / ExifIFDPointer / GPSInfoIFDPointer の 3 件 |
/// | 50..68 | Exif IFD: DateTimeOriginal の 1 件 |
/// | 68..98 | GPS IFD: GPSLatitudeRef / GPSLatitude の 2 件 |
/// | 98.. | 4 バイトに収まらない値の置き場 |
///
/// エントリはタグ番号の昇順に並べる(TIFF の要求)。すべてビッグエンディアン。
fn minimal_exif(description: &str) -> Vec<u8> {
    const IFD0: u32 = 8;
    const EXIF_IFD: u32 = 50;
    const GPS_IFD: u32 = 68;
    const DATA: u32 = 98;

    // 値の置き場。ASCII は末尾 NUL を含める。
    let mut description_bytes = description.as_bytes().to_vec();
    description_bytes.push(0);
    let description_offset = DATA;
    // 次の値は偶数境界に置く(TIFF の慣習)。
    let datetime_offset = align2(description_offset + description_bytes.len() as u32);
    let datetime = b"2026:09:14 12:34:56\0";
    let latitude_offset = align2(datetime_offset + datetime.len() as u32);

    let mut tiff = Vec::new();
    tiff.extend_from_slice(b"MM");
    tiff.extend_from_slice(&0x002Au16.to_be_bytes());
    tiff.extend_from_slice(&IFD0.to_be_bytes());

    // --- IFD0 (offset 8) ---
    tiff.extend_from_slice(&3u16.to_be_bytes());
    entry(
        &mut tiff,
        0x010E,
        2,
        description_bytes.len() as u32,
        description_offset,
    );
    entry(&mut tiff, 0x8769, 4, 1, EXIF_IFD);
    entry(&mut tiff, 0x8825, 4, 1, GPS_IFD);
    tiff.extend_from_slice(&0u32.to_be_bytes()); // 次の IFD は無い
    assert_eq!(tiff.len() as u32, EXIF_IFD);

    // --- Exif IFD (offset 50) ---
    tiff.extend_from_slice(&1u16.to_be_bytes());
    entry(&mut tiff, 0x9003, 2, datetime.len() as u32, datetime_offset);
    tiff.extend_from_slice(&0u32.to_be_bytes());
    assert_eq!(tiff.len() as u32, GPS_IFD);

    // --- GPS IFD (offset 68) ---
    tiff.extend_from_slice(&2u16.to_be_bytes());
    // GPSLatitudeRef は 2 バイトなので値フィールドに直接入る(左詰め)。
    tiff.extend_from_slice(&0x0001u16.to_be_bytes());
    tiff.extend_from_slice(&2u16.to_be_bytes());
    tiff.extend_from_slice(&2u32.to_be_bytes());
    tiff.extend_from_slice(b"N\0\0\0");
    entry(&mut tiff, 0x0002, 5, 3, latitude_offset);
    tiff.extend_from_slice(&0u32.to_be_bytes());
    assert_eq!(tiff.len() as u32, DATA);

    // --- 値の置き場 ---
    tiff.extend_from_slice(&description_bytes);
    while (tiff.len() as u32) < datetime_offset {
        tiff.push(0);
    }
    tiff.extend_from_slice(datetime);
    while (tiff.len() as u32) < latitude_offset {
        tiff.push(0);
    }
    // 35° 41' 21.36" = 北緯 35.6893 度あたり。
    for (num, den) in [(35u32, 1u32), (41, 1), (2136, 100)] {
        tiff.extend_from_slice(&num.to_be_bytes());
        tiff.extend_from_slice(&den.to_be_bytes());
    }
    tiff
}

fn align2(offset: u32) -> u32 {
    offset + (offset % 2)
}

/// IFD エントリ 1 件(12 バイト)。`value` は 4 バイトに収まる値かオフセット。
fn entry(out: &mut Vec<u8>, tag: u16, field_type: u16, count: u32, value: u32) {
    out.extend_from_slice(&tag.to_be_bytes());
    out.extend_from_slice(&field_type.to_be_bytes());
    out.extend_from_slice(&count.to_be_bytes());
    out.extend_from_slice(&value.to_be_bytes());
}

/// JPEG の SOI 直後に `Exif\0\0` + TIFF ブロックを載せた APP1 セグメントを挿入する。
fn jpeg_with_exif(jpeg: &[u8], tiff: &[u8]) -> Vec<u8> {
    assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "入力は JPEG であること");
    let payload_len = 2 + 6 + tiff.len();
    assert!(payload_len <= u16::MAX as usize);

    let mut out = Vec::with_capacity(jpeg.len() + payload_len + 2);
    out.extend_from_slice(&[0xFF, 0xD8]);
    out.extend_from_slice(&[0xFF, 0xE1]);
    out.extend_from_slice(&(payload_len as u16).to_be_bytes());
    out.extend_from_slice(b"Exif\0\0");
    out.extend_from_slice(tiff);
    out.extend_from_slice(&jpeg[2..]);
    out
}
