// このファイルは Phase B の参照実装: 演算順序を目で追えることが最優先のため、
// イテレータ化を促す lint と 16 進リテラルの桁区切り指摘は意図的に抑制する。
#![allow(clippy::needless_range_loop, clippy::unusual_byte_groupings)]

//! f32 リニアライトパイプライン・クロスプラットフォーム決定論スパイク(v0.2)
//!
//! ROADMAP.md「v0.4 — 画素エンジン v2(Phase B)」の最大の技術リスクである
//! **「f32 パイプラインは macOS arm64 / Linux x86_64・arm64 で本当にビット同一の
//! 結果を返すか」を、Phase B 本体に着手する前に小さく検証するためのスパイクである。
//!
//! ここで実装するのは atx-core の実処理とは無関係な、独立した縮小版パイプライン
//! (self-contained。`src/` には一切触れない)。 CI(`.github/workflows/ci.yml`)は
//! macos-14(Apple Silicon = arm64)と ubuntu-24.04(x86_64)の両方でこのテストを走らせる。
//! **両アームで以下の pinned sha256 が一致して初めて「クロスプラットフォーム決定論」が
//! 実証されたことになる**(このファイル単体・ローカル1アームの green だけでは証明にならない)。
//!
//! # なぜズレるリスクがあるのか(このテストが遮断する3つの穴)
//!
//! 1. **libm 差**: `f64::powf` 等の超越関数はプラットフォームの libm 実装に委ねられ、
//!    OS・アーキテクチャ間で最終ビットが 1ULP 前後ずれることがある。
//!    → 対策: 超越関数は「ルックアップテーブルの構築時」にのみ f64 で呼び、
//!    その結果を粗い格子(1e-9 / 1e-6)に量子化してから固定小数のテーブルとして
//!    埋め込む。実行時(画素ループ内)には超越関数を一切呼ばない。
//!    量子化格子は libm の典型的な精度(相対誤差 ~1e-15 = 数 ULP)より十分粗いので、
//!    実装差はほぼ確実に同じ量子化後の値に丸め込まれる。
//! 2. **FMA(融合積和)**: `f32::mul_add` やコンパイラが自動生成する FMA 命令は
//!    「乗算→(丸めずに)加算→1回だけ丸める」ため、「乗算→丸め→加算→丸める」の
//!    2回丸めと最終ビットが異なりうる。FMA 命令の実際の発行はターゲット
//!    (x86_64 の AVX2/FMA3 対応有無、aarch64 の NEON FMA 既定挙動)に依存するため、
//!    同じソースでも CPU アーキテクチャによって結果が変わる典型パターンである。
//!    → 対策: 画素演算では `mul_add` を一切使わず、乗算と加算を明示的に分離した
//!    2文で書く。本ファイル末尾の `mul_add_can_diverge_from_separate_mul_then_add`
//!    がこの乗除算の丸め差を実測で示す。
//! 3. **再結合(reassociation)**: 浮動小数点の加算は結合則を満たさないため、
//!    `a+b+c` の評価順序(左結合か木構造か、SIMD 自動ベクトル化がどう総和を
//!    再構成するか)が変わると最終ビットが変わりうる。
//!    → 対策: 総和は常に固定順序(走査順そのまま、左結合)で書く。
//!    Rust の `+` 演算子はコンパイラが `-ffast-math` 相当のフラグを渡さない限り
//!    式の評価順序を変えない(IEEE754 準拠)ため、ソースの記述順が結果の順序を
//!    決める。本ファイルは同じ数式を「アキュムレータ変数へのループ加算」と
//!    「1本の展開済み加算式」という異なる書き方で二重実装し、両者がビット同一に
//!    なることを固定順序遵守の証拠として確認する。
//!
//! # パイプライン構成
//!
//! 256x256 の決定論的合成 RGBA8 画像(LCG生成)に対して:
//! (a) u8 → f32 の sRGB EOTF 線形化(256 エントリ、f64 計算・1e-9 量子化 LUT)
//! (b) 173x131(意図的に半端な比率)へのバイリニア縮小、固定順序 f32 演算
//! (c) 3x3 コンボリューション(f64 計算・1e-6 量子化の重み、f32 固定順序累算)
//! (d) f32 線形 → u8 sRGB OETF(4096 エントリの量子化逆引き LUT。方式選定は後述)
//! (e) half-away-from-zero での u8 丸め
//!
//! # (d) の方式選定: 二分探索 vs 量子化逆引き LUT
//!
//! 採用: **4096 エントリの量子化逆引き LUT**。
//! 二分探索(順方向 EOTF LUT を対象に line探索して逆引き)も決定論的にはなるが、
//! (1) 実装が複雑になり本スパイクの主眼(丸め規則の明文化)がぼやける、
//! (2) 探索の停止条件(何 ULP で打ち切るか)自体が新しい量子化パラメータになり、
//!     結局は別の LUT 相当の設計判断を追加で背負うだけで得るものが無い。
//! 直接の逆関数(sRGB OETF の解析式)を f64 で計算し、EOTF LUT と同じ量子化戦略
//! (今回は 1e-9 グリッドを流用)を適用したテーブルを作るほうが、コード量・
//! 決定論の主張の両方でシンプルになる。Phase B 本実装でも同じ結論を踏襲する予定。

use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// 決定論的疑似乱数(LCG)。std のどの RNG にも依存しない、完全に自前の生成器。
// ---------------------------------------------------------------------------

/// Numerical Recipes 系の 64bit LCG。実装依存の乱数クレートを使わないことで、
/// 「画像生成そのもの」も含めてこのファイル単体で完全に再現可能にする。
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Lcg(seed)
    }

    fn next_u32(&mut self) -> u32 {
        // 定数は Knuth 由来の定番 LCG 乗数(PCG 系の初期化にも使われる値)。
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 32) as u32
    }

    fn next_u8(&mut self) -> u8 {
        (self.next_u32() >> 24) as u8
    }

    /// [1.0, 2.0) の一様乱数(仮数部だけを乱数で埋めるビット構成なので NaN/Inf を生まない)。
    fn next_f32_one_to_two(&mut self) -> f32 {
        let bits = 0x3f80_0000 | (self.next_u32() & 0x007f_ffff);
        f32::from_bits(bits)
    }
}

const SRC_W: usize = 256;
const SRC_H: usize = 256;
const DST_W: usize = 173; // 意図的に半端な比率(256 との比が単純な分数にならない)
const DST_H: usize = 131;

/// LCG で決定論的に RGBA8 の合成画像を生成する(擬似写真ではなく純粋な数値パターン)。
fn generate_synthetic_image() -> Vec<[u8; 4]> {
    let mut rng = Lcg::new(0x5EED_F32_0000_0001);
    (0..SRC_W * SRC_H)
        .map(|_| [rng.next_u8(), rng.next_u8(), rng.next_u8(), rng.next_u8()])
        .collect()
}

// ---------------------------------------------------------------------------
// (a) sRGB EOTF: u8 -> 線形 f32、f64 計算・1e-9 グリッド量子化
// ---------------------------------------------------------------------------

/// 値を 1e-9 グリッドに丸める(libm 差の遮断)。
///
/// f64 の `powf` 等は数 ULP(相対誤差でおよそ 1e-15〜1e-16)の実装差が
/// プラットフォーム間で起こりうるが、1e-9 という粗い格子に丸めることで
/// その差はほぼ確実に同じ量子化後の値へ吸収される
/// (atx-core の recipe 正規化が f64 を 1e-6 グリッドへ量子化するのと同じ発想。
/// `crates/atx-core/src/recipe.rs` の `quantize` を参照)。
fn quantize_1e9(v: f64) -> f64 {
    (v * 1e9).round() / 1e9
}

fn quantize_1e6(v: f64) -> f64 {
    (v * 1e6).round() / 1e6
}

/// sRGB EOTF(電気信号 → 光信号、= デコード時の線形化)。標準の区分関数。
fn srgb_eotf(c: f64) -> f64 {
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// sRGB OETF(光信号 → 電気信号、= エンコード時の逆変換)。EOTF の解析的逆関数。
fn srgb_oetf(linear: f64) -> f64 {
    if linear <= 0.003_130_8 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    }
}

/// 256 エントリの EOTF LUT(u8 → 線形 f32)。f64 で計算し 1e-9 量子化してから f32 化する。
fn build_eotf_lut() -> [f32; 256] {
    let mut lut = [0f32; 256];
    for (i, slot) in lut.iter_mut().enumerate() {
        let c = i as f64 / 255.0;
        *slot = quantize_1e9(srgb_eotf(c)) as f32;
    }
    lut
}

/// 4096 エントリの逆 LUT(線形 f32 → sRGB f32、0..=1 の量子化空間)。
/// 実行時に超越関数を呼ばないための唯一の入口。
fn build_oetf_inverse_lut() -> [f32; 4096] {
    let mut lut = [0f32; 4096];
    for (i, slot) in lut.iter_mut().enumerate() {
        let linear = i as f64 / 4095.0;
        *slot = quantize_1e9(srgb_oetf(linear)) as f32;
    }
    lut
}

// ---------------------------------------------------------------------------
// (e) half-away-from-zero での u8 丸め
// ---------------------------------------------------------------------------

/// 0.5 をちょうど跨ぐ場合は常に 0 から遠い方へ丸める(banker's rounding ではない)。
/// atx-core の `pixel_ops` が採用しているのと同じ丸め規則を明文化する
/// (round-to-even だと環境やコンパイラ最適化で往復挙動が揺れやすいため、
/// Phase B でも half-away-from-zero を固定規則として踏襲する)。
fn round_half_away_from_zero_u8(v: f32) -> u8 {
    let clamped = v.clamp(0.0, 255.0);
    let rounded = if clamped >= 0.0 {
        (clamped + 0.5).floor()
    } else {
        (clamped - 0.5).ceil()
    };
    rounded.clamp(0.0, 255.0) as u8
}

fn linear_to_srgb_u8(linear: f32, oetf_lut: &[f32; 4096]) -> u8 {
    let clamped = linear.clamp(0.0, 1.0);
    // インデックス丸めは LUT の格子点選択のみに使う(最終画素丸めは (e) で別途行う)。
    let idx = (clamped * 4095.0).round() as usize;
    let srgb = oetf_lut[idx.min(4095)];
    round_half_away_from_zero_u8(srgb * 255.0)
}

// ---------------------------------------------------------------------------
// (b) バイリニア縮小: 256x256 -> 173x131、固定順序 f32 演算
// ---------------------------------------------------------------------------

/// pixel-center 整列(`(o+0.5)*scale - 0.5`)のバイリニア補間。
/// 4隅の重み付き和は常に「x方向を先に補間 → y方向を補間」の順で、
/// 各ステップの乗算・加算を分離して書く(`mul_add` は使わない = FMA 禁止)。
fn resize_bilinear(src: &[[f32; 4]], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<[f32; 4]> {
    let mut out = vec![[0f32; 4]; dw * dh];
    let scale_x = sw as f32 / dw as f32;
    let scale_y = sh as f32 / dh as f32;

    for oy in 0..dh {
        let sy = ((oy as f32 + 0.5) * scale_y - 0.5).max(0.0);
        let sy0f = sy.floor();
        let fy = sy - sy0f;
        let y0 = (sy0f as usize).min(sh - 1);
        let y1 = (y0 + 1).min(sh - 1);

        for ox in 0..dw {
            let sx = ((ox as f32 + 0.5) * scale_x - 0.5).max(0.0);
            let sx0f = sx.floor();
            let fx = sx - sx0f;
            let x0 = (sx0f as usize).min(sw - 1);
            let x1 = (x0 + 1).min(sw - 1);

            for c in 0..4 {
                let p00 = src[y0 * sw + x0][c];
                let p10 = src[y0 * sw + x1][c];
                let p01 = src[y1 * sw + x0][c];
                let p11 = src[y1 * sw + x1][c];

                // 明示的に「乗算」「加算」を分離した文で書く(mul_add 不使用)。
                let top = p00 * (1.0 - fx) + p10 * fx;
                let bottom = p01 * (1.0 - fx) + p11 * fx;
                let value = top * (1.0 - fy) + bottom * fy;

                out[oy * dw + ox][c] = value;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// (c) 3x3 コンボリューション: f64 計算・1e-6 量子化の重み、f32 固定順序累算
// ---------------------------------------------------------------------------

/// 正規化ガウス風カーネル(合計 1.0)。重みは f64 で書き下し、1e-6 グリッドへ
/// 量子化してから f32 化する(atx-core の recipe 量子化と同じ精度契約)。
fn build_conv_weights() -> [[f32; 3]; 3] {
    let raw: [[f64; 3]; 3] = [
        [1.0 / 16.0, 2.0 / 16.0, 1.0 / 16.0],
        [2.0 / 16.0, 4.0 / 16.0, 2.0 / 16.0],
        [1.0 / 16.0, 2.0 / 16.0, 1.0 / 16.0],
    ];
    let mut out = [[0f32; 3]; 3];
    for j in 0..3 {
        for i in 0..3 {
            out[j][i] = quantize_1e6(raw[j][i]) as f32;
        }
    }
    out
}

fn clamp_index(v: isize, max_exclusive: usize) -> usize {
    v.clamp(0, max_exclusive as isize - 1) as usize
}

/// 実装1: アキュムレータ変数へループで加算していく素朴な実装。
/// 走査順は ky=0..3(行) × kx=0..3(列)、行優先で固定。
fn convolve3x3_loop(img: &[[f32; 4]], w: usize, h: usize, kernel: &[[f32; 3]; 3]) -> Vec<[f32; 4]> {
    let mut out = vec![[0f32; 4]; w * h];
    for y in 0..h {
        for x in 0..w {
            for c in 0..4 {
                let mut acc: f32 = 0.0;
                for ky in 0..3 {
                    let sy = clamp_index(y as isize + ky as isize - 1, h);
                    for kx in 0..3 {
                        let sx = clamp_index(x as isize + kx as isize - 1, w);
                        let pixel = img[sy * w + sx][c];
                        let weight = kernel[ky][kx];
                        // 乗算(丸め1回)→ 加算(丸め1回)を明示的に分離。mul_add 禁止。
                        let term = pixel * weight;
                        acc += term;
                    }
                }
                out[y * w + x][c] = acc;
            }
        }
    }
    out
}

/// 実装2: 同じ走査順(ky=0..3 × kx=0..3、行優先)を1本の展開済み加算式として
/// 独立に書き下したもの。Rust の `+` は左結合で評価順序が固定されるため、
/// ループ版とバイト単位で一致するはずである。これが一致しなければ、
/// 「加算の再結合(reassociation)がどこかで起きている」ことを意味する
/// 回帰検知になる。
fn convolve3x3_unrolled(
    img: &[[f32; 4]],
    w: usize,
    h: usize,
    kernel: &[[f32; 3]; 3],
) -> Vec<[f32; 4]> {
    let mut out = vec![[0f32; 4]; w * h];
    for y in 0..h {
        for x in 0..w {
            let px = |dy: isize, dx: isize, c: usize| -> f32 {
                let sy = clamp_index(y as isize + dy, h);
                let sx = clamp_index(x as isize + dx, w);
                img[sy * w + sx][c]
            };
            for c in 0..4 {
                #[rustfmt::skip]
                let acc =
                      px(-1, -1, c) * kernel[0][0] + px(-1, 0, c) * kernel[0][1] + px(-1, 1, c) * kernel[0][2]
                    + px( 0, -1, c) * kernel[1][0] + px( 0, 0, c) * kernel[1][1] + px( 0, 1, c) * kernel[1][2]
                    + px( 1, -1, c) * kernel[2][0] + px( 1, 0, c) * kernel[2][1] + px( 1, 1, c) * kernel[2][2];
                out[y * w + x][c] = acc;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// パイプライン全体の組み立て
// ---------------------------------------------------------------------------

type ConvolveFn = fn(&[[f32; 4]], usize, usize, &[[f32; 3]; 3]) -> Vec<[f32; 4]>;

fn run_pipeline(convolve: ConvolveFn) -> Vec<u8> {
    let src_u8 = generate_synthetic_image();
    let eotf_lut = build_eotf_lut();
    let oetf_lut = build_oetf_inverse_lut();

    // (a) 線形化。RGB は EOTF LUT を経由し、アルファは規約どおり非線形変換の
    // 対象外(すでに線形なストレートアルファ)として 0..1 へスケールするのみ。
    let linear: Vec<[f32; 4]> = src_u8
        .iter()
        .map(|p| {
            [
                eotf_lut[p[0] as usize],
                eotf_lut[p[1] as usize],
                eotf_lut[p[2] as usize],
                p[3] as f32 / 255.0,
            ]
        })
        .collect();

    // (b) バイリニア縮小
    let resized = resize_bilinear(&linear, SRC_W, SRC_H, DST_W, DST_H);

    // (c) 3x3 コンボリューション
    let kernel = build_conv_weights();
    let convolved = convolve(&resized, DST_W, DST_H, &kernel);

    // (d) + (e) OETF 逆変換 + half-away-from-zero 丸め
    let mut out = Vec::with_capacity(DST_W * DST_H * 4);
    for p in &convolved {
        out.push(linear_to_srgb_u8(p[0], &oetf_lut));
        out.push(linear_to_srgb_u8(p[1], &oetf_lut));
        out.push(linear_to_srgb_u8(p[2], &oetf_lut));
        // アルファは非線形変換しないので、そのまま 0..255 へ丸め戻す。
        out.push(round_half_away_from_zero_u8(p[3].clamp(0.0, 1.0) * 255.0));
    }
    out
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

/// 本体テスト: pinned sha256 との一致。
///
/// この値は macOS arm64 でのローカル実行で確定させた(1回目は placeholder で
/// 実行し、実測値を採取してこの定数に書き戻した)。**このテストが CI の
/// macos-14(arm64)・ubuntu-24.04(x86_64)の両方で green になって初めて、
/// 「f32 パイプラインはクロスプラットフォームで決定論的である」という
/// Phase B 着手の前提が実証されたことになる。** ローカル(mac arm64)一本での
/// green はこの前提の必要条件でしかなく、十分条件ではない。
#[test]
fn f32_pipeline_hash_is_pinned() {
    let out = run_pipeline(convolve3x3_loop);
    let hash = sha256_hex(&out);
    assert_eq!(
        hash, "4a7006e092609c4409bd40370e8327d4f3b578ecfdd93129cbb59bc68f5dd8be",
        "f32 spike pipeline output hash changed. If this is an intentional algorithm change, \
         re-run locally, capture the new hash, and update this constant with a note explaining \
         why. If it is unintentional, this is exactly the kind of drift Phase B must never ship."
    );
}

/// FMA・再結合ガード: ループ版とアンロール版(同じ演算順序を独立に書いたもの)が
/// ビット同一であることを確認する。乖離があれば、どちらかの実装が意図せず
/// `mul_add` 相当の融合積和や異なる加算順序を持ち込んだことを意味する。
#[test]
fn f32_pipeline_conv_loop_and_unrolled_agree_bit_exact() {
    let out_loop = run_pipeline(convolve3x3_loop);
    let out_unrolled = run_pipeline(convolve3x3_unrolled);
    assert_eq!(
        out_loop, out_unrolled,
        "convolve3x3_loop と convolve3x3_unrolled は同じ演算順序のはずなのに \
         結果が食い違った(reassociation もしくは暗黙の FMA 混入を疑うこと)"
    );
}

/// mul_add(真の FMA)と「乗算→加算」の分離実装は一般に異なる結果を返しうることを
/// 実測で示す(乱数探索。決め打ちの定数に依存しないことで環境非依存に成立させる)。
/// これが「画素パイプラインで mul_add を使わない」規律の実証的根拠であり、
/// 万一将来誰かがパフォーマンス目的で `mul_add` を導入しようとしたときに、
/// なぜダメなのかをこのテストが具体的に示す。
#[test]
fn mul_add_can_diverge_from_separate_mul_then_add() {
    let mut rng = Lcg::new(0xF5A0_0000_0001_u64);
    let mut found = false;
    for _ in 0..100_000 {
        let a = rng.next_f32_one_to_two();
        let b = rng.next_f32_one_to_two();
        let c = -rng.next_f32_one_to_two();
        let fma = a.mul_add(b, c);
        let separate = a * b + c;
        if fma != separate {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "expected to find at least one (a, b, c) where f32::mul_add diverges from a*b+c \
         within 100,000 random samples in [1,2); if this fails, either the search space is too \
         small or the platform's FMA lowering changed in a way worth re-investigating"
    );
}
