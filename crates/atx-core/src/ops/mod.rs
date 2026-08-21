//! v0.2 以降の op 実装モジュール群。
//!
//! 分割方針: 1モジュール = 1委譲単位(並列実装時の衝突回避)。
//! - `perspective`: 射影変換(+ transform.rs の射影対応と連動)
//! - `color`: color_matrix / curves / levels(トーン・カラー系メタ op)
//! - `blur`: blur / median / unsharp_mask(カーネル系)
//!
//! 決定論の規約(全モジュール共通):
//! - libm 由来の関数(exp, pow 等)の結果を画素計算に直接使わない。
//!   カーネル・LUT は f64 で計算後、1e-6 グリッドへ量子化してから適用する
//!   (プラットフォーム間の 1 ULP 差を遮断する。recipe.rs の canonical 量子化と同思想)
//! - トーン系は 256 エントリ LUT を経由し、画素ループは整数演算に閉じる

pub mod blur;
pub mod color;
pub mod perspective;
