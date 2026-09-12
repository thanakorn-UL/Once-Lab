//! The sRGB transfer function, shared by every renderer stage that needs it.
//!
//! Both the RGBTable stage ([`super::apply`]) and the LookTable stage
//! ([`super::look_table_apply`]) bracket their lookup in the sRGB opto-electronic
//! transfer function, so the curve lives here exactly once rather than being
//! copy-pasted per stage (a divergence between the copies would silently change
//! one stage's domain).

/// sRGB transfer encode (linear -> encoded). The input is expected to already be
/// clamped to [0,1].
pub(super) fn srgb_encode(linear: f32) -> f32 {
    if linear <= 0.0031308 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    }
}

/// sRGB transfer decode (encoded -> linear). The input is expected to already be
/// clamped to [0,1].
pub(super) fn srgb_decode(encoded: f32) -> f32 {
    if encoded <= 0.04045 {
        encoded / 12.92
    } else {
        ((encoded + 0.055) / 1.055).powf(2.4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_round_trip_is_exact_enough() {
        for value in [0.0f32, 0.001, 0.0031308, 0.01, 0.18, 0.5, 1.0] {
            let round_trip = srgb_decode(srgb_encode(value));
            assert!(
                (round_trip - value).abs() < 1e-6,
                "round trip of {value} produced {round_trip}"
            );
        }
    }
}
