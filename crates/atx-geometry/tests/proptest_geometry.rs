//! プロパティテスト: 傾き検出の決定論・出力範囲(DESIGN §5-6)。

use atx_geometry::{detect_tilt, DetectParams};
use image::{DynamicImage, GrayImage};
use proptest::prelude::*;

fn arb_gray_image() -> impl Strategy<Value = GrayImage> {
    (16u32..128, 16u32..128).prop_flat_map(|(w, h)| {
        prop::collection::vec(any::<u8>(), (w * h) as usize)
            .prop_map(move |data| GrayImage::from_raw(w, h, data).expect("exact buffer size"))
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 16, .. ProptestConfig::default() })]

    /// 13. detect_tilt はパニックせず決定論的(同一入力→同一シリアライズ結果)であり、
    ///     confidence は 0..=1 に収まり、recommended_angle_degrees が Some のときは
    ///     max_abs_angle 以内かつ alternatives のいずれかに近い。
    #[test]
    fn detect_tilt_is_deterministic_and_bounded(gray in arb_gray_image()) {
        let image = DynamicImage::ImageLuma8(gray);
        let params = DetectParams::default();

        let r1 = detect_tilt(&image, &params);
        let r2 = detect_tilt(&image, &params);

        let j1 = serde_json::to_string(&r1).unwrap();
        let j2 = serde_json::to_string(&r2).unwrap();
        prop_assert_eq!(j1, j2, "detect_tilt must be deterministic for identical input");

        prop_assert!(
            (0.0..=1.0).contains(&r1.confidence),
            "confidence out of range: {}",
            r1.confidence
        );

        if let Some(angle) = r1.recommended_angle_degrees {
            prop_assert!(
                angle.abs() <= params.max_abs_angle + 1e-6,
                "recommended angle {angle} exceeds max_abs_angle {}",
                params.max_abs_angle
            );
            prop_assert!(
                !r1.alternatives.is_empty(),
                "a recommended angle must be backed by at least one alternative"
            );
            let close = r1
                .alternatives
                .iter()
                .any(|c| (c.angle_degrees - angle).abs() <= 5.0);
            prop_assert!(
                close,
                "recommended angle {angle} is not near any alternative: {:?}",
                r1.alternatives
            );
        }
    }
}
