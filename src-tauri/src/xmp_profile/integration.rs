//! Bridges an already-parsed XMP RGB profile onto developed RAW pixels.
//!
//! The RAW layer never reads or parses XMP itself; it receives an
//! [`XmpRgbProfile`] as data. Application happens on the developed
//! `Intermediate::ThreeColor` buffer, whose values are LINEAR sRGB / D65, which
//! is exactly the domain of the Adobe RGBTable renderer.

use super::apply::RgbTableApplyContext;
use crate::xmp_profile::XmpRgbProfile;

/// Resolves the runtime RGBTable amount for `profile`.
///
/// `None` leaves the profile exactly as authored. `Some(percent)` applies
/// Once-Lab's compatibility model for the Lightroom Profile Amount slider: the
/// authored amount is scaled linearly by the percent, so `100` renders exactly
/// as authored, `0` is identity, and `200` doubles the authored strength. The
/// result is clamped to the range the table declares as supported, which stays
/// authoritative for every valid input.
pub(crate) fn effective_profile_amount(
    profile: &XmpRgbProfile,
    ui_percent: Option<f32>,
) -> Result<f32, String> {
    let base = profile.rgb_table_amount.unwrap_or(1.0);

    let Some(percent) = ui_percent else {
        return Ok(base);
    };

    // The control is only meaningful for a profile that declares it is scalable.
    if !profile.supports_amount {
        return Err("XMP profile does not support Amount adjustment".to_string());
    }

    // A percent outside the advertised range is a caller error, never silently
    // clamped: only the table's own amount range may clamp a valid percent.
    if !percent.is_finite() || !(0.0..=200.0).contains(&percent) {
        return Err("invalid XMP profile amount percent".to_string());
    }

    let (min_amount, max_amount) = (profile.table.min_amount, profile.table.max_amount);

    if !min_amount.is_finite()
        || !max_amount.is_finite()
        || !(0.0..=1.0).contains(&min_amount)
        || max_amount < 1.0
    {
        return Err("invalid RGBTable amount bounds".to_string());
    }

    // The product is formed in f64 so a percent of 100 round-trips the authored
    // amount bit-exactly instead of re-rounding it through f32.
    Ok((f64::from(base) * f64::from(percent) / 100.0).clamp(min_amount, max_amount) as f32)
}

/// Applies `profile` to every pixel in place, honouring an optional Amount control.
///
/// `ui_percent` is the raw slider value; the authored amount, the table bounds
/// and the scaling all stay owned by the backend.
pub(crate) fn apply_profile_to_three_color_pixels_with_amount(
    pixels: &mut [[f32; 3]],
    profile: &XmpRgbProfile,
    ui_percent: Option<f32>,
) -> Result<(), String> {
    let amount = effective_profile_amount(profile, ui_percent)?;

    let context = RgbTableApplyContext::new(&profile.table, amount)?;

    for pixel in pixels.iter_mut() {
        *pixel = context.apply(*pixel)?;
    }

    Ok(())
}

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
    apply_profile_to_three_color_pixels_with_amount(pixels, profile, None)
}

#[cfg(test)]
mod acceptance {
    use std::path::PathBuf;
    use std::time::Instant;

    use image::GenericImageView;

    use crate::app_settings::AppSettings;

    /// The tolerance every "must reproduce" requirement of the milestone is
    /// pinned to. It is always compared in raw float space, never against a
    /// channel-clamped conversion, so an overrange regression stays visible.
    const ACCEPTANCE_TOLERANCE: f32 = 1e-6;

    /// A deviation big enough to be worth failing over at all. Used by the
    /// sanity checks, which have to prove that something actually happened
    /// (a profile that changes nothing, or an Amount slider that is ignored,
    /// must show up here).
    const MEANINGFUL_DEVIATION: f32 = 1e-3;

    /// Spatial reach of the raw enhancement that runs AFTER the profile stage in
    /// `image_loader::load_base_image_from_bytes_with_xmp_profile`: the colour-NR
    /// pass samples row/column offsets `[-5, -1, 3]` and the detail enhance blurs
    /// over a radius of 2 pixels, so clamping one pixel can legitimately shift the
    /// finished render of anything within five pixels of it.
    const ENHANCEMENT_RADIUS: i32 = 5;

    fn stats(image: &image::DynamicImage) -> (f64, f64, f64, [u8; 3]) {
        let rgb = image.to_rgb8();
        let (w, h) = rgb.dimensions();
        let mut sums = [0.0f64; 3];
        let mut count = 0.0f64;
        for (_, _, p) in rgb.enumerate_pixels() {
            for c in 0..3 {
                sums[c] += f64::from(p[c]);
            }
            count += 1.0;
        }
        let mean = [sums[0] / count, sums[1] / count, sums[2] / count];
        let center = rgb.get_pixel(w / 2, h / 2).0;
        (mean[0], mean[1], mean[2], center)
    }

    fn max_delta(a: &image::DynamicImage, b: &image::DynamicImage) -> f32 {
        let (ra, rb) = (raw_f32(a), raw_f32(b));
        assert_eq!(ra.len(), rb.len());
        let mut worst = 0.0f32;
        for (pa, pb) in ra.iter().zip(rb.iter()) {
            for c in 0..3 {
                worst = worst.max((pa[c] - pb[c]).abs());
            }
        }
        worst
    }


    /// Raw float pixels with NO conversion, so values outside [0,1] stay visible
    /// (`DynamicImage::to_rgb32f` clamps, which would hide linear overrange).
    fn raw_f32(img: &image::DynamicImage) -> Vec<[f32; 3]> {
        use image::DynamicImage as D;

        match img {
            D::ImageRgba32F(buffer) => buffer.pixels().map(|p| [p[0], p[1], p[2]]).collect(),
            D::ImageRgb32F(buffer) => buffer.pixels().map(|p| [p[0], p[1], p[2]]).collect(),
            other => other.to_rgb32f().pixels().map(|p| [p[0], p[1], p[2]]).collect(),
        }
    }

    struct Diff {
        in_range_worst: f32,
        in_range_count: usize,
        outside_worst: f32,
        outside_count: usize,
        peak: f32,
        trough: f32,
        brighter: usize,
        darker: usize,
        max_pos: f32,
        max_neg: f32,
        worst: ([f32; 3], [f32; 3]),
    }

    /// Compares `a` against `reference` in raw float space, splitting by whether
    /// the reference pixel lies inside [0,1].
    ///
    /// NOTE: on the *finished* image this split always leaves the outside bucket
    /// empty, because `remove_raw_artifacts_and_enhance` clamps its output to
    /// [0,1]; it is descriptive evidence about the delivered picture. The split
    /// that actually decides whether 0% can round-trip is the one produced by
    /// [`stage_reproduction`], which works on the profile stage's own input.
    fn diff(a: &image::DynamicImage, reference: &image::DynamicImage) -> Diff {
        let (ra, rr) = (raw_f32(a), raw_f32(reference));
        assert_eq!(ra.len(), rr.len());

        let mut d = Diff {
            in_range_worst: 0.0,
            in_range_count: 0,
            outside_worst: 0.0,
            outside_count: 0,
            peak: f32::MIN,
            trough: f32::MAX,
            brighter: 0,
            darker: 0,
            max_pos: 0.0,
            max_neg: 0.0,
            worst: ([0.0; 3], [0.0; 3]),
        };

        for (pa, pr) in ra.iter().zip(rr.iter()) {
            for c in 0..3 {
                d.peak = d.peak.max(pr[c]);
                d.trough = d.trough.min(pr[c]);
            }

            let outside = pr.iter().any(|c| *c > 1.0 || *c < 0.0);
            let worst_channel = (0..3).map(|c| (pa[c] - pr[c]).abs()).fold(0.0f32, f32::max);
            let mean = (0..3).map(|c| pa[c] - pr[c]).sum::<f32>() / 3.0;

            if outside {
                d.outside_count += 1;
                d.outside_worst = d.outside_worst.max(worst_channel);
            } else {
                d.in_range_count += 1;
                d.in_range_worst = d.in_range_worst.max(worst_channel);
            }

            if mean > 1e-6 {
                d.brighter += 1;
            } else if mean < -1e-6 {
                d.darker += 1;
            }

            d.max_pos = d.max_pos.max(mean);
            if mean < d.max_neg {
                d.max_neg = mean;
                d.worst = (*pa, *pr);
            }
        }

        d
    }

    /// How well a 0% stage render reproduces the no-profile stage input.
    ///
    /// The stage's `gamut_clip` metadata clamps its linear input to [0,1] before
    /// the table, so a pixel whose input channel already left [0,1] provably
    /// cannot round-trip: the only value it may render is that input clamped per
    /// channel. The pixels that stay inside [0,1] (where 0% must reproduce the
    /// baseline exactly) and the ones that do not (where the clamp must be the
    /// whole story) are therefore measured separately.
    struct StageReproduction {
        in_range_worst: f32,
        in_range_count: usize,
        in_range_raw_worst: f32,
        outside_worst: f32,
        outside_count: usize,
        outside_raw_worst: f32,
    }

    fn stage_reproduction(rendered: &[[f32; 3]], stage_input: &[[f32; 3]]) -> StageReproduction {
        assert_eq!(rendered.len(), stage_input.len());

        let mut reproduction = StageReproduction {
            in_range_worst: 0.0,
            in_range_count: 0,
            in_range_raw_worst: 0.0,
            outside_worst: 0.0,
            outside_count: 0,
            outside_raw_worst: 0.0,
        };

        for (render, input) in rendered.iter().zip(stage_input.iter()) {
            let outside = input.iter().any(|c| *c > 1.0 || *c < 0.0);

            // What the stage is expected to render: the input clamped per channel.
            let clamped_worst = (0..3)
                .map(|channel| (render[channel] - input[channel].clamp(0.0, 1.0)).abs())
                .fold(0.0f32, f32::max);

            // What an unclamped pipeline would have had to reproduce instead.
            let raw_worst = (0..3)
                .map(|channel| (render[channel] - input[channel]).abs())
                .fold(0.0f32, f32::max);

            if outside {
                reproduction.outside_count += 1;
                reproduction.outside_worst = reproduction.outside_worst.max(clamped_worst);
                reproduction.outside_raw_worst = reproduction.outside_raw_worst.max(raw_worst);
            } else {
                reproduction.in_range_count += 1;
                reproduction.in_range_worst = reproduction.in_range_worst.max(clamped_worst);
                reproduction.in_range_raw_worst =
                    reproduction.in_range_raw_worst.max(raw_worst);
            }
        }

        reproduction
    }

    /// Splits the *finished* render's deviation from the no-profile baseline at
    /// 0% into what the `gamut_clip` clamp can explain and what it cannot.
    ///
    /// The clamp can only change a pixel whose stage input left [0,1]; the
    /// downstream raw enhancement then spreads that change over its
    /// [`ENHANCEMENT_RADIUS`]-pixel spatial support. Any other deviating pixel
    /// would be a deviation the clamp does not account for.
    struct ClampAttribution {
        explained: usize,
        unexplained: usize,
        unexplained_worst: f32,
    }

    fn clamp_attribution(
        rendered: &[[f32; 3]],
        baseline: &[[f32; 3]],
        stage_input: &[[f32; 3]],
        width: usize,
        height: usize,
        threshold: f32,
    ) -> ClampAttribution {
        assert_eq!(rendered.len(), width * height);
        assert_eq!(rendered.len(), baseline.len());
        assert_eq!(rendered.len(), stage_input.len());

        // Exactly the pixels the clamp is allowed to touch.
        let mut clamped = vec![false; width * height];
        for (index, input) in stage_input.iter().enumerate() {
            clamped[index] = input.iter().any(|c| *c > 1.0 || *c < 0.0);
        }

        // Chebyshev dilation of the clamped set: horizontal sweep, then vertical.
        let radius = ENHANCEMENT_RADIUS;
        let mut horizontally_reached = vec![false; width * height];
        for y in 0..height {
            let row = y * width;
            for x in 0..width {
                horizontally_reached[row + x] = (-radius..=radius).any(|offset| {
                    let sx = (x as i32 + offset).clamp(0, width as i32 - 1) as usize;
                    clamped[row + sx]
                });
            }
        }

        let mut reached = vec![false; width * height];
        for y in 0..height {
            for x in 0..width {
                reached[y * width + x] = (-radius..=radius).any(|offset| {
                    let sy = (y as i32 + offset).clamp(0, height as i32 - 1) as usize;
                    horizontally_reached[sy * width + x]
                });
            }
        }

        let mut attribution = ClampAttribution {
            explained: 0,
            unexplained: 0,
            unexplained_worst: 0.0,
        };

        for index in 0..width * height {
            let worst = (0..3)
                .map(|channel| (rendered[index][channel] - baseline[index][channel]).abs())
                .fold(0.0f32, f32::max);

            if worst <= threshold {
                continue;
            }

            if reached[index] {
                attribution.explained += 1;
            } else {
                attribution.unexplained += 1;
                attribution.unexplained_worst = attribution.unexplained_worst.max(worst);
            }
        }

        attribution
    }

    #[test]
    fn real_profile_acceptance() {
        // The fixture directory is UNTRACKED, so a clean checkout has to skip
        // this test instead of failing the suite. Nothing may run before this
        // guard: canonicalizing the missing directory would panic.
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../raw-filewithxmp-profile");

        if !fixture.is_dir() {
            eprintln!("ACCEPTANCE SKIPPED: {} is missing", fixture.display());
            return;
        }

        // The directory is known to exist, so canonicalizing it (and the usual
        // `expect`) is safe from here on.
        let dir = fixture.canonicalize().expect("acceptance directory");
        let nef = dir.join("TLP_8278.NEF");
        if !nef.exists() {
            eprintln!("ACCEPTANCE SKIPPED: {} missing", nef.display());
            return;
        }

        // The profiles live in the same untracked directory: a partial fixture
        // has to skip for exactly the same reason a missing NEF does.
        for profile_name in ["Au ⛏️.xmp", "Cu ⛏️.xmp"] {
            let profile_file = dir.join(profile_name);
            if !profile_file.exists() {
                eprintln!("ACCEPTANCE SKIPPED: {} missing", profile_file.display());
                return;
            }
        }

        let bytes = std::fs::read(&nef).expect("read NEF");
        let settings = AppSettings::default();
        let path_str = nef.to_string_lossy().to_string();

        let baseline = crate::image_loader::load_base_image_from_bytes_with_xmp_profile(
            &bytes, &path_str, false, &settings, None, None, None,
        )
        .expect("baseline develop");
        let (br, bg, bb, bc) = stats(&baseline);
        let baseline_pixels = raw_f32(&baseline);

        println!(
            "ACCEPT|baseline|center={:?}|mean=[{:.6},{:.6},{:.6}]",
            bc, br, bg, bb
        );

        // The profile stage consumes the linear ThreeColor buffer produced during
        // RAW development, i.e. BEFORE the `remove_raw_artifacts_and_enhance`
        // step the loader runs afterwards. That buffer - not the finished,
        // already clamped and spatially filtered picture - is the domain in which
        // "0% reproduces the no-profile baseline" is defined, so rebuild it
        // exactly the way the loader does.
        let highlight_compression = settings.raw_highlight_compression.unwrap_or(2.5);
        let linear_mode = settings.linear_raw_mode.clone();

        let stage_input = crate::raw_processing::develop_raw_image_with_profile(
            &bytes,
            false,
            highlight_compression,
            linear_mode.clone(),
            None,
            None,
            None,
        )
        .expect("stage input develop");
        let stage_input_pixels = raw_f32(&stage_input);
        let (stage_width, stage_height) = stage_input.dimensions();
        let (stage_width, stage_height) = (stage_width as usize, stage_height as usize);

        for profile_name in ["Au ⛏️.xmp", "Cu ⛏️.xmp"] {
            let profile_file = dir.join(profile_name);
            let label = profile_name.chars().next().unwrap();
            let profile = crate::xmp_profile::load_xmp_rgb_profile_from_path(&profile_file)
                .expect("load acceptance profile");

            let authored = crate::image_loader::load_base_image_from_bytes_with_xmp_profile(
                &bytes,
                &path_str,
                false,
                &settings,
                Some(profile_file.as_path()),
                None,
                None,
            )
            .expect("authored develop");

            let (ar, ag, ab, ac) = stats(&authored);
            println!(
                "ACCEPT|{}|authored|center={:?}|mean=[{:.6},{:.6},{:.6}]|vs_baseline={}",
                label,
                ac,
                ar,
                ag,
                ab,
                max_delta(&authored, &baseline)
            );

            // Kept so the "the Amount slider is not ignored" check below can
            // compare the two ends of the range with the usual helper.
            let mut at_zero: Option<image::DynamicImage> = None;
            let mut at_hundred: Option<image::DynamicImage> = None;

            for percent in [0.0f32, 50.0, 100.0, 200.0] {
                let start = Instant::now();

                let rendered = crate::image_loader::load_base_image_from_bytes_with_xmp_profile(
                    &bytes,
                    &path_str,
                    false,
                    &settings,
                    Some(profile_file.as_path()),
                    Some(percent),
                    None,
                )
                .expect("scaled develop");

                let elapsed = start.elapsed();
                let vs_baseline = max_delta(&rendered, &baseline);
                let vs_authored = max_delta(&rendered, &authored);

                let (r, g, b, c) = stats(&rendered);
                println!(
                    "ACCEPT|{}|{}|center={:?}|mean=[{:.6},{:.6},{:.6}]|vs_baseline={}|vs_authored={}|ms={}",
                    label,
                    percent,
                    c,
                    r,
                    g,
                    b,
                    vs_baseline,
                    vs_authored,
                    elapsed.as_millis()
                );

                if percent == 100.0 {
                    // HARD REQUIREMENT: 100% *is* the authored render, so it must
                    // reproduce the no-amount render, not merely resemble it.
                    // Compared on raw floats, with no channel clamping anywhere.
                    assert!(
                        vs_authored <= ACCEPTANCE_TOLERANCE,
                        "{} at 100%: must reproduce the authored (no-amount) render within {ACCEPTANCE_TOLERANCE}, but max delta vs authored was {vs_authored}",
                        label
                    );

                    // Non-vacuity: this profile has to change the picture,
                    // otherwise "100% reproduces the authored render" would hold
                    // trivially and prove nothing.
                    assert!(
                        vs_baseline > MEANINGFUL_DEVIATION,
                        "{} at 100%: the profile must actually change the image, but max delta vs the no-profile baseline was only {vs_baseline}",
                        label
                    );
                }

                if percent == 0.0 {
                    let d = diff(&rendered, &baseline);
                    println!(
                        "ACCEPT|{}|0-diff|in_range_max_delta={:.9}|in_range_pixels={}|outside_max_delta={:.9}|outside_pixels={}|baseline_peak={:.6}|baseline_trough={:.6}",
                        label,
                        d.in_range_worst,
                        d.in_range_count,
                        d.outside_worst,
                        d.outside_count,
                        d.peak,
                        d.trough
                    );
                    println!(
                        "ACCEPT|{}|0-worst|brighter={}|darker={}|max_mean_pos={:.6}|max_mean_neg={:.6}|worst_render={:?}|worst_baseline={:?}",
                        label, d.brighter, d.darker, d.max_pos, d.max_neg, d.worst.0, d.worst.1
                    );

                    // HARD REQUIREMENT: 0% must reproduce the no-profile baseline
                    // except for the one difference the pipeline genuinely forces
                    // - the profile stage's gamut_clip clamp of linear input to
                    // [0,1]. That exception only exists on the stage's own input
                    // buffer (the finished image is clamped again downstream), so
                    // it is asserted on that buffer.
                    let stage_zero = crate::raw_processing::develop_raw_image_with_profile(
                        &bytes,
                        false,
                        highlight_compression,
                        linear_mode.clone(),
                        Some(&profile),
                        Some(0.0),
                        None,
                    )
                    .expect("stage 0% develop");
                    let stage_zero_pixels = raw_f32(&stage_zero);

                    let reproduction = stage_reproduction(&stage_zero_pixels, &stage_input_pixels);
                    println!(
                        "ACCEPT|{}|0-stage|in_range_max_delta={:.9}|in_range_pixels={}|outside_max_delta={:.9}|outside_pixels={}|in_range_unclamped={:.9}|outside_unclamped={:.9}",
                        label,
                        reproduction.in_range_worst,
                        reproduction.in_range_count,
                        reproduction.outside_worst,
                        reproduction.outside_count,
                        reproduction.in_range_raw_worst,
                        reproduction.outside_raw_worst
                    );

                    // Both buckets must be populated: an "in range only" fixture
                    // would silently skip the clamp case, and an "outside only"
                    // one would mean the stage is a no-op everywhere else.
                    assert!(
                        reproduction.in_range_count > 0 && reproduction.outside_count > 0,
                        "{} at 0%: the stage input must contain both in-range and outside-[0,1] pixels, got in={} outside={}",
                        label,
                        reproduction.in_range_count,
                        reproduction.outside_count
                    );

                    // Inside [0,1] the clamp cannot do anything, so 0% has to
                    // reproduce the no-profile baseline exactly.
                    assert!(
                        reproduction.in_range_worst <= ACCEPTANCE_TOLERANCE,
                        "{} at 0%: pixels whose stage input stayed inside [0,1] must reproduce the no-profile baseline within {ACCEPTANCE_TOLERANCE}, but max delta was {}",
                        label,
                        reproduction.in_range_worst
                    );

                    // Outside [0,1] the clamp is the only admissible difference,
                    // so re-deriving the baseline pixel with a per-channel clamp
                    // must reproduce the render.
                    assert!(
                        reproduction.outside_worst <= ACCEPTANCE_TOLERANCE,
                        "{} at 0%: for pixels whose stage input left [0,1], the per-channel clamped baseline must reproduce the render within {ACCEPTANCE_TOLERANCE}, but max delta was {}",
                        label,
                        reproduction.outside_worst
                    );

                    // ...and that clamp must genuinely be what those pixels lost,
                    // i.e. the render must NOT match the unclamped input.
                    assert!(
                        reproduction.outside_raw_worst > ACCEPTANCE_TOLERANCE,
                        "{} at 0%: the outside-[0,1] bucket is not caused by the clamp - the unclamped baseline already reproduced the render (max delta {})",
                        label,
                        reproduction.outside_raw_worst
                    );

                    // Finally, account for the finished picture: its deviation
                    // must sit entirely within the enhancement's spatial reach of
                    // a clamped pixel, leaving nothing unexplained.
                    let attribution = clamp_attribution(
                        &raw_f32(&rendered),
                        &baseline_pixels,
                        &stage_input_pixels,
                        stage_width,
                        stage_height,
                        ACCEPTANCE_TOLERANCE,
                    );
                    println!(
                        "ACCEPT|{}|0-clamp-attribution|explained_pixels={}|unexplained_pixels={}|unexplained_worst={:.9}",
                        label,
                        attribution.explained,
                        attribution.unexplained,
                        attribution.unexplained_worst
                    );

                    assert!(
                        attribution.unexplained == 0,
                        "{} at 0%: {} final pixels deviate from the no-profile baseline without a clamped pixel within the enhancement's {}-pixel reach (worst {})",
                        label,
                        attribution.unexplained,
                        ENHANCEMENT_RADIUS,
                        attribution.unexplained_worst
                    );

                    assert!(
                        attribution.explained > 0,
                        "{} at 0%: the clamp must visibly reach the final image, otherwise this attribution check is vacuous",
                        label
                    );

                    at_zero = Some(rendered);
                } else if percent == 100.0 {
                    at_hundred = Some(rendered);
                }
            }

            // An "Amount slider is ignored" bug would render both ends of the
            // range identically; they must not be identical.
            let at_zero = at_zero.expect("0% render was produced");
            let at_hundred = at_hundred.expect("100% render was produced");
            let zero_vs_hundred = max_delta(&at_zero, &at_hundred);
            println!("ACCEPT|{label}|0-vs-100|max_delta={zero_vs_hundred}");
            assert!(
                zero_vs_hundred > MEANINGFUL_DEVIATION,
                "{label} at 0% must differ from {label} at 100% by more than {MEANINGFUL_DEVIATION}, but max delta was only {zero_vs_hundred}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xmp_profile::RgbTable;
    use crate::xmp_profile::apply_rgb_table;

    /// Non-identity: every node maps to a constant encoded value.
    const CONSTANT_2X2X2: [[f32; 3]; 8] = [[0.25, 0.5, 0.75]; 8];

    fn make_profile(values: [[f32; 3]; 8], amount: Option<f32>) -> XmpRgbProfile {
        make_bounded_profile(values, amount, 0.0, 1.0, true)
    }

    fn make_bounded_profile(
        values: [[f32; 3]; 8],
        amount: Option<f32>,
        min_amount: f64,
        max_amount: f64,
        supports_amount: bool,
    ) -> XmpRgbProfile {
        XmpRgbProfile {
            name: "Test Profile".to_string(),
            group: None,
            uuid: "TEST123".to_string(),
            process_version: None,
            supports_amount,
            convert_to_grayscale: false,
            rgb_table_id: "TESTTABLE".to_string(),
            rgb_table_amount: amount,
            table: RgbTable {
                size: 2,
                values: values.to_vec(),
                color_space: 0,
                gamma: 1,
                gamut: 0,
                min_amount,
                max_amount,
            },
        }
    }

    /// Pixels inside [0,1] spanning the table domain.
    const SAMPLE_PIXELS: [[f32; 3]; 5] = [
        [0.0, 0.25, 0.5],
        [0.18, 0.5, 0.9],
        [1.0, 1.0, 0.0],
        [0.5, 0.5, 0.5],
        [0.02, 0.75, 0.33],
    ];

    fn assert_close(actual: f32, expected: f32, tolerance: f32, label: &str) {
        assert!(
            (actual - expected).abs() <= tolerance,
            "{label}: expected {expected}, got {actual}"
        );
    }

    #[test]
    fn effective_amount_none_preserves_authored_amount() {
        // Au-like: authored 0.5 on the real Au bounds.
        let au = make_bounded_profile(CONSTANT_2X2X2, Some(0.5), 0.0, 1.5, true);
        assert_eq!(effective_profile_amount(&au, None).unwrap(), 0.5);

        // Cu-like: authored 0.6 on the real Cu bounds.
        let cu = make_bounded_profile(CONSTANT_2X2X2, Some(0.6), 0.0, 1.6, true);
        assert_eq!(effective_profile_amount(&cu, None).unwrap(), 0.6);
    }

    #[test]
    fn effective_amount_scales_authored_amount() {
        let au = make_bounded_profile(CONSTANT_2X2X2, Some(0.5), 0.0, 1.5, true);
        let cu = make_bounded_profile(CONSTANT_2X2X2, Some(0.6), 0.0, 1.6, true);

        for (percent, expected) in [(0.0, 0.0), (50.0, 0.25), (100.0, 0.5), (200.0, 1.0)] {
            assert_close(
                effective_profile_amount(&au, Some(percent)).unwrap(),
                expected,
                1e-7,
                &format!("Au at {percent}%"),
            );
        }

        for (percent, expected) in [(0.0, 0.0), (50.0, 0.3), (100.0, 0.6), (200.0, 1.2)] {
            assert_close(
                effective_profile_amount(&cu, Some(percent)).unwrap(),
                expected,
                1e-7,
                &format!("Cu at {percent}%"),
            );
        }
    }

    #[test]
    fn effective_amount_respects_table_max() {
        // Raw product 0.95 * 2 = 1.9 exceeds the table maximum.
        let profile = make_bounded_profile(CONSTANT_2X2X2, Some(0.95), 0.0, 1.5, true);
        assert_close(
            effective_profile_amount(&profile, Some(200.0)).unwrap(),
            1.5,
            1e-7,
            "clamped to max_amount",
        );
    }

    #[test]
    fn effective_amount_respects_nonzero_table_min() {
        // Raw product 0.5 * 0 = 0 falls below a table that starts at 0.25.
        let profile = make_bounded_profile(CONSTANT_2X2X2, Some(0.5), 0.25, 1.5, true);
        assert_close(
            effective_profile_amount(&profile, Some(0.0)).unwrap(),
            0.25,
            1e-7,
            "clamped to min_amount",
        );
    }

    #[test]
    fn rejects_invalid_profile_amount_percent() {
        let profile = make_bounded_profile(CONSTANT_2X2X2, Some(0.5), 0.0, 1.5, true);

        for percent in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -1.0, 201.0] {
            let error = effective_profile_amount(&profile, Some(percent))
                .expect_err("an out-of-range percent must be rejected");
            assert_eq!(error, "invalid XMP profile amount percent");
        }
    }

    #[test]
    fn rejects_amount_override_when_profile_does_not_support_amount() {
        let profile = make_bounded_profile(CONSTANT_2X2X2, Some(0.5), 0.0, 1.5, false);

        let error = effective_profile_amount(&profile, Some(50.0))
            .expect_err("a non-supporting profile must reject an override");
        assert_eq!(error, "XMP profile does not support Amount adjustment");

        // Without an override the profile still renders at its authored amount.
        assert_eq!(effective_profile_amount(&profile, None).unwrap(), 0.5);
    }

    #[test]
    fn profile_amount_100_matches_authored_profile_render() {
        let profile = make_bounded_profile(CONSTANT_2X2X2, Some(0.6), 0.0, 1.6, true);

        let mut authored = SAMPLE_PIXELS.to_vec();
        apply_profile_to_three_color_pixels(&mut authored, &profile).expect("authored render");

        let mut at_100 = SAMPLE_PIXELS.to_vec();
        apply_profile_to_three_color_pixels_with_amount(&mut at_100, &profile, Some(100.0))
            .expect("100% render");

        for (index, (a, b)) in authored.iter().zip(at_100.iter()).enumerate() {
            for channel in 0..3 {
                assert_close(
                    b[channel],
                    a[channel],
                    1e-7,
                    &format!("pixel {index} channel {channel}"),
                );
            }
        }
    }

    /// The "100% preserves the pre-3C rendering" claim, checked against code this
    /// milestone never touched.
    ///
    /// Comparing the two new entry points against each other proves only that
    /// they agree with themselves: a change that altered both identically would
    /// still pass. Pre-3C code rendered every pixel by building
    /// `RgbTableApplyContext::new(table, rgb_table_amount.unwrap_or(1.0))` and
    /// calling `apply`, which is exactly what [`apply_rgb_table`] still does. So
    /// the untouched primitive is the independent reference: both the no-amount
    /// path and 100% - which is defined to be the authored no-amount render -
    /// must reproduce it pixel for pixel.
    #[test]
    fn no_amount_path_matches_the_pre_3c_per_pixel_renderer() {
        let profile = make_bounded_profile(CONSTANT_2X2X2, Some(0.6), 0.0, 1.6, true);
        let authored_amount = profile.rgb_table_amount.unwrap_or(1.0);

        // Straddle the table domain, including the 0.0 and 1.0 corners the
        // 2x2x2 lookup interpolates between, plus the shared sample pixels.
        let inputs: Vec<[f32; 3]> = vec![
            [0.0, 0.0, 0.0],
            [1.0, 1.0, 1.0],
            [0.0, 1.0, 0.5],
            SAMPLE_PIXELS[0],
            SAMPLE_PIXELS[1],
            SAMPLE_PIXELS[2],
            SAMPLE_PIXELS[3],
            SAMPLE_PIXELS[4],
        ];

        // Non-vacuity: the table has to change at least one sample pixel,
        // otherwise every equivalence below would hold for an identity table.
        let mut changes_a_pixel = false;
        for pixel in inputs.iter() {
            let reference = apply_rgb_table(*pixel, &profile.table, authored_amount)
                .expect("reference apply");
            if (0..3).any(|channel| (reference[channel] - pixel[channel]).abs() > 1e-4) {
                changes_a_pixel = true;
                break;
            }
        }
        assert!(
            changes_a_pixel,
            "the test profile must actually change at least one sample pixel by more than 1e-4, otherwise the pre-3C equivalence is vacuous"
        );

        // The no-amount path must equal the untouched pre-3C primitive.
        let mut no_amount = inputs.clone();
        apply_profile_to_three_color_pixels(&mut no_amount, &profile).expect("no-amount render");
        for (index, pixel) in inputs.iter().enumerate() {
            let reference = apply_rgb_table(*pixel, &profile.table, authored_amount)
                .expect("reference apply");
            for channel in 0..3 {
                assert_close(
                    no_amount[index][channel],
                    reference[channel],
                    1e-7,
                    &format!("no-amount pixel {index} channel {channel}"),
                );
            }
        }

        // 100% is defined to *be* the authored render, so it must reproduce the
        // same untouched primitive rather than merely the no-amount path.
        let mut at_100 = inputs.clone();
        apply_profile_to_three_color_pixels_with_amount(&mut at_100, &profile, Some(100.0))
            .expect("100% render");
        for (index, pixel) in inputs.iter().enumerate() {
            let reference = apply_rgb_table(*pixel, &profile.table, authored_amount)
                .expect("reference apply");
            for channel in 0..3 {
                assert_close(
                    at_100[index][channel],
                    reference[channel],
                    1e-7,
                    &format!("100% pixel {index} channel {channel}"),
                );
            }
        }
    }

    #[test]
    fn profile_amount_zero_is_identity_when_table_min_is_zero() {
        // A clearly non-identity table, so identity can only come from the amount.
        let profile = make_bounded_profile(CONSTANT_2X2X2, Some(0.5), 0.0, 1.5, true);

        let mut pixels = SAMPLE_PIXELS.to_vec();
        apply_profile_to_three_color_pixels_with_amount(&mut pixels, &profile, Some(0.0))
            .expect("0% render");

        for (index, (out, input)) in pixels.iter().zip(SAMPLE_PIXELS.iter()).enumerate() {
            for channel in 0..3 {
                assert_close(
                    out[channel],
                    input[channel],
                    1e-6,
                    &format!("pixel {index} channel {channel}"),
                );
            }
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

    /// Slice used by the amount-layer mutation tests: deliberately non-empty,
    /// with values the constant table would visibly change.
    fn amount_mutation_pixels() -> Vec<[f32; 3]> {
        vec![[0.18, 0.5, 0.9], [0.9, 0.4, 0.1], [0.25, 0.25, 0.25]]
    }

    #[test]
    fn invalid_amount_percent_does_not_partially_mutate_pixels() {
        // The amount layer must reject a bad slider value before the first real
        // pixel is rendered, exactly like an invalid profile does. Both a NaN
        // and a percent above the advertised 200% ceiling qualify.
        let profile = make_bounded_profile(CONSTANT_2X2X2, Some(0.5), 0.0, 1.5, true);

        for percent in [f32::NAN, 201.0] {
            let mut pixels = amount_mutation_pixels();
            let original = pixels.clone();

            let error = apply_profile_to_three_color_pixels_with_amount(
                &mut pixels,
                &profile,
                Some(percent),
            )
            .expect_err("an out-of-range percent must be rejected");

            assert_eq!(error, "invalid XMP profile amount percent");
            assert_eq!(
                pixels, original,
                "pixels must be untouched when percent {percent} is rejected"
            );
        }
    }

    #[test]
    fn amount_override_without_amount_support_does_not_partially_mutate_pixels() {
        // A profile that does not declare `SupportsAmount` must reject the
        // override before rendering, leaving the slice bit-identical.
        let profile = make_bounded_profile(CONSTANT_2X2X2, Some(0.5), 0.0, 1.5, false);

        let mut pixels = amount_mutation_pixels();
        let original = pixels.clone();

        let error =
            apply_profile_to_three_color_pixels_with_amount(&mut pixels, &profile, Some(50.0))
                .expect_err("a non-supporting profile must reject the override");

        assert_eq!(error, "XMP profile does not support Amount adjustment");
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
