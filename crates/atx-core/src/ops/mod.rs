//! op 実装モジュール群。
//!
//! 分割方針: 1モジュール = 1委譲単位(並列実装時の衝突回避)。
//! - `perspective`: 射影変換(+ transform.rs の射影対応と連動)
//! - `color`: color_matrix / curves / levels(トーン・カラー系メタ op)
//! - `blur`: blur / median / unsharp_mask(カーネル系)
//! - `convolve` / `hsl` / `lut` / `wb`
//! - `mask`: 局所適用マスク(重み平面の解決とブレンド。v0.5、DESIGN.md §9.6)
//! - `blend`: レイヤー合成(W3C ブレンド 16 種 = separable 12 + 非 separable 4。
//!   v0.6 / v0.7、DESIGN.md §9.7 / §9.8)
//! - `clone_heal`: clone / heal(円領域の複写・修復。v0.7、DESIGN.md §9.8)
//! - `svg`: svg_overlay(SVG のラスタライズと焼き込み。v0.8、DESIGN.md §9.9)
//!
//! # 画素エンジン v2: op ごとの作業空間(v0.4 の中核設計)
//!
//! パイプラインの中間表現は `crate::linear::LinearImage`(f32 RGBA、ストレートアルファ)
//! ひとつだが、**その数値をどう解釈するか(作業空間)は op ごとに異なる**。
//! エンジンは現在の空間を遅延追跡し、必要になった瞬間にだけ変換を挟む
//! (同じ空間の op が連続するときは往復させない = 変換誤差も計算コストも積まない)。
//!
//! | 作業空間 | op | 理由 |
//! |---|---|---|
//! | **線形光** | `resize` / `rotate` / `perspective` / `crop` / `pad` / `blur` / `median` / `unsharp_mask` / `convolve` / `white_balance` / `clone` / `heal` | 画素の**混合**(加重平均・畳み込み)と**露出のスケール**は、物理的な光量に対して行ってはじめて正しい。符号値のまま平均すると暗部に寄る(古典的なガンマ・ブラー誤差) |
//! | **sRGB 符号値** | `adjust` / `color_matrix` / `curves` / `levels` / `hsl` / `lut` / `svg_overlay` | これらは「見た目のトーンカーブ」を操作する語彙で、スライダの効き方・制御点の座標・`.cube` の定義域がいずれも**符号値**上の慣習で決まっている。線形光で適用すると同じ数値が全く違う見た目になる |
//!
//! 空間変換は `linear::srgb_to_linear` / `linear::linear_to_srgb`(4096 エントリ +
//! 線形補間の量子化 LUT)を双方向で使う。u8 の格子上では往復がバイト同一になる
//! ことを `linear` のユニットテストが硬いゲートとして固定している。
//!
//! # アルファの扱い
//!
//! **画素を混ぜる op(`resize` / `rotate` / `perspective` / `blur` / `convolve`)は、
//! フィルタ前にアルファをプリマルチプライし、後で解く。**
//! 解くときの規則は `a < 1e-6` なら RGB を 0(0 除算とノイズ増幅の回避)。
//! アルファが全画素 1.0 の画像ではプリマルチプライは 1.0 倍 = 厳密な恒等なので、
//! 「アルファなし画像の高速パス」は専用コードを持たなくてもビット同一になる。
//! `median` は非線形フィルタで加重平均を取らないため、プリマルチプライの対象外
//! (チャンネルごとに独立した順序統計量を取る)。
//!
//! # 決定論の規約(全モジュール共通。`tests/f32_spike.rs` が参照実装)
//!
//! - libm 由来の関数(exp, pow, sin 等)の結果を画素計算に直接使わない。
//!   カーネル・LUT は f64 で計算後、1e-6(係数)/ 1e-9(伝達関数)グリッドへ
//!   量子化してから適用する
//! - `f32::mul_add`(FMA)を使わない。乗算と加算は別の文/式に分ける
//! - 総和は走査順そのままの左結合で書く(再結合禁止)
//! - 行分割の並列化は画素間の実行順序しか変えないので出力に影響しない

pub mod blend;
pub mod blur;
pub mod clone_heal;
pub mod color;
pub mod convolve;
pub mod hsl;
pub mod lut;
pub mod mask;
pub mod perspective;
pub mod svg;
pub mod wb;
