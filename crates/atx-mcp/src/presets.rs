//! ビルトインプリセット(= 名前付きレシピ)。
//!
//! ROADMAP §Agent UX の規律 #3「プリセット = 語彙の圧縮」の実装。
//! `presets/*.json` をコンパイル時に埋め込み、`apply_transform` / `render_preview` の
//! `preset` 引数から解決する。
//!
//! **プリセットは純粋な糖衣である**: 解決後は通常のレシピとして既存パイプラインを
//! 流れ、`recipe_hash` は**解決後のレシピ**に対して計算される。したがって
//! 「preset 名で呼んだ場合」と「同じ内容の生レシピで呼んだ場合」は同一 revision に
//! 落ちる(冪等キーはプリセット名に依存しない)。プリセットの中身を変更すると
//! 新しいハッシュになるが、既存 revision の再現性は台帳のレシピ本体で保証される。

use atx_core::TransformRecipe;
use serde::Deserialize;

/// 埋め込まれたプリセット JSON(名前 → ファイル内容)。
///
/// 新しいプリセットを足すときは `presets/<name>.json` を作り、この表に1行足す。
/// 表の名前と JSON の `"name"` が一致することはユニットテストで検証される。
pub const PRESET_FILES: &[(&str, &str)] = &[
    (
        "architecture_clean",
        include_str!("../../../presets/architecture_clean.json"),
    ),
    (
        "bw_high_contrast",
        include_str!("../../../presets/bw_high_contrast.json"),
    ),
    (
        "bw_neutral",
        include_str!("../../../presets/bw_neutral.json"),
    ),
    (
        "bw_red_filter",
        include_str!("../../../presets/bw_red_filter.json"),
    ),
    ("bw_soft", include_str!("../../../presets/bw_soft.json")),
    (
        "cinema_teal_orange",
        include_str!("../../../presets/cinema_teal_orange.json"),
    ),
    (
        "duotone_navy_cream",
        include_str!("../../../presets/duotone_navy_cream.json"),
    ),
    (
        "eyecatch_16_9",
        include_str!("../../../presets/eyecatch_16_9.json"),
    ),
    ("film_cool", include_str!("../../../presets/film_cool.json")),
    (
        "film_grain_strong",
        include_str!("../../../presets/film_grain_strong.json"),
    ),
    ("film_soft", include_str!("../../../presets/film_soft.json")),
    ("film_warm", include_str!("../../../presets/film_warm.json")),
    (
        "food_vivid",
        include_str!("../../../presets/food_vivid.json"),
    ),
    (
        "grain_fine",
        include_str!("../../../presets/grain_fine.json"),
    ),
    ("grayscale", include_str!("../../../presets/grayscale.json")),
    ("hero_2400", include_str!("../../../presets/hero_2400.json")),
    (
        "instagram_portrait_4_5",
        include_str!("../../../presets/instagram_portrait_4_5.json"),
    ),
    (
        "instagram_square_1080",
        include_str!("../../../presets/instagram_square_1080.json"),
    ),
    (
        "landscape_punch",
        include_str!("../../../presets/landscape_punch.json"),
    ),
    (
        "matte_fade",
        include_str!("../../../presets/matte_fade.json"),
    ),
    (
        "og_1200x630",
        include_str!("../../../presets/og_1200x630.json"),
    ),
    (
        "portrait_soft",
        include_str!("../../../presets/portrait_soft.json"),
    ),
    (
        "product_clean",
        include_str!("../../../presets/product_clean.json"),
    ),
    (
        "product_white",
        include_str!("../../../presets/product_white.json"),
    ),
    ("sepia", include_str!("../../../presets/sepia.json")),
    (
        "soft_vignette",
        include_str!("../../../presets/soft_vignette.json"),
    ),
    (
        "thumbnail_square",
        include_str!("../../../presets/thumbnail_square.json"),
    ),
    (
        "web_optimize",
        include_str!("../../../presets/web_optimize.json"),
    ),
    (
        "x_wide_16_9",
        include_str!("../../../presets/x_wide_16_9.json"),
    ),
    (
        "youtube_thumb_1280x720",
        include_str!("../../../presets/youtube_thumb_1280x720.json"),
    ),
];

/// プリセット JSON の中身。
#[derive(Debug, Clone, Deserialize)]
pub struct Preset {
    pub name: String,
    pub description: String,
    pub recipe: TransformRecipe,
}

/// プリセット名の一覧(表の登録順 = 辞書順)。
pub fn preset_names() -> Vec<&'static str> {
    PRESET_FILES.iter().map(|(name, _)| *name).collect()
}

/// 埋め込み JSON をすべてパースして返す。パースに失敗するプリセットは
/// ビルド時点のバグなのでユニットテストで弾く(実行時は該当プリセットを飛ばす)。
pub fn all() -> Vec<Preset> {
    PRESET_FILES
        .iter()
        .filter_map(|(_, json)| serde_json::from_str::<Preset>(json).ok())
        .collect()
}

/// 名前からプリセットを解決する。未知の名前・壊れた JSON は `Err(理由)`。
pub fn resolve(name: &str) -> Result<Preset, PresetError> {
    let (_, json) = PRESET_FILES
        .iter()
        .find(|(preset_name, _)| *preset_name == name)
        .ok_or(PresetError::Unknown)?;
    serde_json::from_str::<Preset>(json).map_err(|e| PresetError::Malformed(e.to_string()))
}

/// [`resolve`] の失敗理由。
#[derive(Debug, Clone)]
pub enum PresetError {
    /// そんな名前のプリセットはない。
    Unknown,
    /// 埋め込み JSON が壊れている(ビルド時のバグ)。
    Malformed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_preset_parses_and_matches_its_table_name() {
        for (name, json) in PRESET_FILES {
            let preset: Preset = serde_json::from_str(json)
                .unwrap_or_else(|e| panic!("preset {name} must be valid JSON: {e}"));
            assert_eq!(
                &preset.name, name,
                "preset file name and its \"name\" field must match"
            );
            assert!(
                !preset.description.trim().is_empty(),
                "preset {name} needs a description"
            );
            assert!(
                !preset.recipe.operations.is_empty(),
                "preset {name} must have at least one operation"
            );
        }
    }

    #[test]
    fn preset_names_are_sorted_and_unique() {
        let names = preset_names();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(names, sorted, "PRESET_FILES must be sorted and unique");
    }

    /// v0.1 op のみで構成されたプリセットは、いま atx-core の validate を通る。
    #[test]
    fn v1_only_presets_pass_core_validate() {
        for name in ["eyecatch_16_9", "thumbnail_square", "web_optimize"] {
            let preset = resolve(name).expect("preset must resolve");
            atx_core::recipe::validate(&preset.recipe)
                .unwrap_or_else(|e| panic!("preset {name} must be a valid recipe: {e}"));
        }
    }

    /// color_matrix / curves 系プリセットの validate(v0.2 の color op で実装済み)。
    #[test]
    fn color_presets_pass_core_validate() {
        for name in ["film_soft", "grayscale", "sepia"] {
            let preset = resolve(name).expect("preset must resolve");
            atx_core::recipe::validate(&preset.recipe)
                .unwrap_or_else(|e| panic!("preset {name} must be a valid recipe: {e}"));
        }
    }

    /// v0.3 op(white_balance)を使うプリセットの validate。
    #[test]
    fn v03_presets_pass_core_validate() {
        let preset = resolve("product_clean").expect("preset must resolve");
        atx_core::recipe::validate(&preset.recipe)
            .unwrap_or_else(|e| panic!("preset product_clean must be a valid recipe: {e}"));
    }

    #[test]
    fn unknown_preset_is_reported() {
        assert!(matches!(resolve("nope"), Err(PresetError::Unknown)));
    }

    /// v0.3.0 op(gradient_map)を使うプリセットの validate。
    /// gradient_map の validate は着地済み(`ops::gradient::validate_stops`)。
    #[test]
    fn gradient_map_preset_passes_core_validate() {
        let preset = resolve("duotone_navy_cream").expect("preset must resolve");
        atx_core::recipe::validate(&preset.recipe)
            .unwrap_or_else(|e| panic!("preset duotone_navy_cream must be a valid recipe: {e}"));
    }

    /// v0.3.0 op(vignette / grain / auto_levels)を使うプリセット。
    /// これらの `validate` は他 2 エージェントが並行実装中で、この時点では
    /// `todo!()` のため呼ぶとパニックする。ops が着地したら ignore を外すこと。
    #[test]
    fn vignette_grain_auto_levels_presets_pass_core_validate() {
        for name in [
            "film_warm",
            "film_cool",
            "film_grain_strong",
            "grain_fine",
            "portrait_soft",
            "landscape_punch",
            "soft_vignette",
            "product_white",
            "architecture_clean",
        ] {
            let preset = resolve(name).expect("preset must resolve");
            atx_core::recipe::validate(&preset.recipe)
                .unwrap_or_else(|e| panic!("preset {name} must be a valid recipe: {e}"));
        }
    }
}
