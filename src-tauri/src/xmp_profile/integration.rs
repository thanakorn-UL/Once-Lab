//! Bridges an already-parsed XMP RGB profile onto developed RAW pixels.
//!
//! The RAW layer never reads or parses XMP itself; it receives an
//! [`XmpRgbProfile`] as data. Application happens on the developed
//! `Intermediate::ThreeColor` buffer, whose values are LINEAR sRGB / D65, which
//! is exactly the domain of the Adobe RGBTable renderer.

use super::apply::RgbTableApplyContext;
use crate::xmp_profile::XmpRgbProfile;

/// Applies `profile` to every pixel in place.
///
/// The profile is validated exactly once, before the first real pixel is
/// touched, so an invalid profile can never leave the image partially
/// processed and the O(size^3) table validation never runs per pixel. No
/// second image buffer is allocated.
pub(crate) fn apply_profile_to_three_color_pixels(
    pixels: &mut [[f32; 3]],
    profile: &XmpRgbProfile,
) -> Result<(), String> {
    let amount = profile.rgb_table_amount.unwrap_or(1.0);

    let context = RgbTableApplyContext::new(&profile.table, amount)?;

    for pixel in pixels.iter_mut() {
        *pixel = context.apply(*pixel)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xmp_profile::RgbTable;
    use crate::xmp_profile::apply_rgb_table;

    /// Non-identity: every node maps to a constant encoded value.
    const CONSTANT_2X2X2: [[f32; 3]; 8] = [[0.25, 0.5, 0.75]; 8];

    fn make_table(values: [[f32; 3]; 8]) -> RgbTable {
        RgbTable {
            size: 2,
            values: values.to_vec(),
            color_space: 0,
            gamma: 1,
            gamut: 0,
            min_amount: 0.0,
            max_amount: 1.0,
        }
    }

    fn make_profile(values: [[f32; 3]; 8], amount: Option<f32>) -> XmpRgbProfile {
        XmpRgbProfile {
            name: "Test Profile".to_string(),
            group: None,
            uuid: "TEST123".to_string(),
            process_version: None,
            supports_amount: true,
            convert_to_grayscale: false,
            rgb_table_id: "TESTTABLE".to_string(),
            rgb_table_amount: amount,
            table: make_table(values),
        }
    }

    #[test]
    fn profile_application_mutates_pixels_in_place() {
        let profile = make_profile(CONSTANT_2X2X2, Some(1.0));

        let mut pixels = vec![[0.18f32, 0.5, 0.9], [0.9, 0.4, 0.1]];
        let original = pixels.clone();

        apply_profile_to_three_color_pixels(&mut pixels, &profile)
            .expect("valid profile should apply");

        for (index, pixel) in pixels.iter().enumerate() {
            for channel in 0..3 {
                assert!(
                    pixel[channel].is_finite(),
                    "pixel {index} channel {channel} is not finite"
                );
            }

            let changed =
                (0..3).any(|channel| (pixel[channel] - original[index][channel]).abs() > 1e-6);
            assert!(changed, "pixel {index} was not modified");
        }
    }

    #[test]
    fn missing_profile_amount_defaults_to_one() {
        let profile = make_profile(CONSTANT_2X2X2, None);

        let inputs = [[0.18f32, 0.5, 0.9], [0.9, 0.4, 0.1]];
        let mut pixels = inputs.to_vec();

        apply_profile_to_three_color_pixels(&mut pixels, &profile)
            .expect("valid profile should apply");

        for (index, input) in inputs.iter().enumerate() {
            let expected =
                apply_rgb_table(*input, &profile.table, 1.0).expect("direct apply should succeed");

            for channel in 0..3 {
                assert!(
                    (pixels[index][channel] - expected[channel]).abs() < 1e-6,
                    "pixel {index} channel {channel}: expected {}, got {}",
                    expected[channel],
                    pixels[index][channel]
                );
            }
        }
    }

    #[test]
    fn invalid_profile_does_not_partially_mutate_pixels() {
        let mut profile = make_profile(CONSTANT_2X2X2, Some(1.0));
        profile.table.gamma = 0;

        let mut pixels = vec![[0.18f32, 0.5, 0.9], [0.9, 0.4, 0.1], [0.25, 0.25, 0.25]];
        let original = pixels.clone();

        let error = apply_profile_to_three_color_pixels(&mut pixels, &profile)
            .expect_err("invalid profile must be rejected");

        assert_eq!(error, "unsupported RGBTable gamma: 0");
        assert_eq!(pixels, original, "pixels must be untouched on error");
    }

    #[test]
    fn empty_pixel_slice_validates_profile() {
        let valid = make_profile(CONSTANT_2X2X2, Some(1.0));
        let mut empty: [[f32; 3]; 0] = [];

        assert!(
            apply_profile_to_three_color_pixels(&mut empty, &valid).is_ok(),
            "valid profile with no pixels should succeed"
        );

        let mut invalid = make_profile(CONSTANT_2X2X2, Some(1.0));
        invalid.table.gamut = 1;

        assert!(
            apply_profile_to_three_color_pixels(&mut empty, &invalid).is_err(),
            "invalid profile must be rejected even when there are no pixels"
        );
    }
}
