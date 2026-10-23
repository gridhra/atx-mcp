//! .cube の DOMAIN 値の上限(2026-10-22 のレビュー: 有限性の検査の後で 1e-6 グリッドへ
//! 量子化するため、絶対値が約 1.8e302 を超える DOMAIN は ±inf になり、検証も適用も
//! 成功したうえで全画素が NaN → 黒になった)。

use atx_core::validate_cube_asset;

fn cube(domain_max: &str) -> String {
    format!(
        "LUT_3D_SIZE 2\nDOMAIN_MIN 0 0 0\nDOMAIN_MAX {domain_max} {domain_max} {domain_max}\n\
         0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1 1 1\n"
    )
}

#[test]
fn domain_values_that_overflow_the_quantization_grid_are_rejected() {
    assert!(validate_cube_asset(cube("1").as_bytes()).is_ok());
    let reason = validate_cube_asset(cube("1e303").as_bytes()).expect_err("must be refused");
    assert!(reason.contains("DOMAIN"), "{reason}");
}
