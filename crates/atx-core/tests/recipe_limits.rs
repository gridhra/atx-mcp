//! レシピの大きさの上限(2026-10-22 のレビュー: op 数とレイヤー数に上限が無く、
//! 時間が op 数に比例して伸び、マスクのキャッシュが際限なく溜まった)。

use atx_core::recipe::validate;
use atx_core::{AtxError, TransformRecipe};

fn ops(n: usize) -> String {
    let items: Vec<&str> = (0..n).map(|_| r#"{"op":"auto_orient"}"#).collect();
    format!("[{}]", items.join(","))
}

#[test]
fn a_pipeline_may_hold_at_most_128_operations() {
    let ok: TransformRecipe =
        serde_json::from_str(&format!(r#"{{"operations":{}}}"#, ops(128))).unwrap();
    assert!(validate(&ok).is_ok());

    let too_many: TransformRecipe =
        serde_json::from_str(&format!(r#"{{"operations":{}}}"#, ops(129))).unwrap();
    match validate(&too_many) {
        Err(AtxError::InvalidRecipe(message)) => assert!(message.contains("128"), "{message}"),
        other => panic!("expected InvalidRecipe, got {other:?}"),
    }
}

#[test]
fn a_layer_pipeline_has_the_same_operation_limit() {
    let json = format!(
        r#"{{"layers":[{{"source":"base","ops":{}}}],"operations":[]}}"#,
        ops(129)
    );
    let recipe: TransformRecipe = serde_json::from_str(&json).unwrap();
    match validate(&recipe) {
        Err(AtxError::InvalidRecipe(message)) => {
            assert!(
                message.contains("layers[0]") && message.contains("128"),
                "{message}"
            )
        }
        other => panic!("expected InvalidRecipe, got {other:?}"),
    }
}

#[test]
fn a_recipe_may_hold_at_most_32_layers() {
    let layer = r#"{"source":"base","ops":[]}"#;
    let layers = |n: usize| {
        let items: Vec<&str> = (0..n).map(|_| layer).collect();
        format!("[{}]", items.join(","))
    };
    let ok: TransformRecipe =
        serde_json::from_str(&format!(r#"{{"layers":{},"operations":[]}}"#, layers(32))).unwrap();
    assert!(validate(&ok).is_ok(), "{:?}", validate(&ok));

    let too_many: TransformRecipe =
        serde_json::from_str(&format!(r#"{{"layers":{},"operations":[]}}"#, layers(33))).unwrap();
    match validate(&too_many) {
        Err(AtxError::InvalidRecipe(message)) => assert!(message.contains("32"), "{message}"),
        other => panic!("expected InvalidRecipe, got {other:?}"),
    }
}
