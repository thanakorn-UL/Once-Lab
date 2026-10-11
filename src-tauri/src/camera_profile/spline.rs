//! Port of the public DNG SDK `dng_spline_solver` (`dng_spline.cpp`), used to
//! evaluate a camera profile's `ProfileToneCurve` (tag 50940): a C0/C1/C2
//! continuous cubic spline through the authored `(x, y)` knots with zero
//! second derivative at the end points.
//!
//! `dng_render` concatenates this function with the exposure-tone function and
//! samples it into a `dng_1d_table`; we evaluate the solved spline directly,
//! which matches the sampled table to well below output quantization.

/// A solved `dng_spline_solver`: knots `x`,`y` and per-knot slopes `s`.
#[derive(Debug, Clone)]
pub(crate) struct DngSpline {
    x: Vec<f64>,
    y: Vec<f64>,
    s: Vec<f64>,
}

impl DngSpline {
    /// `dng_spline_solver::Solve`. Knots must be strictly increasing in x
    /// (enforced by the caller's validation); fewer than 2 knots is an error.
    pub(crate) fn solve(knots: &[(f64, f64)]) -> Result<Self, String> {
        let count = knots.len();
        if count < 2 {
            return Err("tone curve needs at least 2 knots".to_string());
        }
        for window in knots.windows(2) {
            for knot in window {
                if !knot.0.is_finite() || !knot.1.is_finite() {
                    return Err("tone curve knots must be finite".to_string());
                }
            }
            if window[1].0 <= window[0].0 {
                return Err("tone curve knots must be strictly increasing in x".to_string());
            }
        }

        let x: Vec<f64> = knots.iter().map(|k| k.0).collect();
        let y: Vec<f64> = knots.iter().map(|k| k.1).collect();
        let mut s = vec![0.0f64; count];

        // First: secant/weighted-average slopes, exactly as the SDK.
        let mut a = x[1] - x[0];
        let mut b = (y[1] - y[0]) / a;
        s[0] = b;
        for j in 2..count {
            let c = x[j] - x[j - 1];
            let d = (y[j] - y[j - 1]) / c;
            s[j - 1] = (b * c + d * a) / (a + c);
            a = c;
            b = d;
        }
        s[count - 1] = 2.0 * b - s[count - 2];
        s[0] = 2.0 * s[0] - s[1];

        if count > 2 {
            // Tridiagonal refinement pass (E/F/G) from dng_spline.cpp.
            let mut e = vec![0.0f64; count];
            let mut f = vec![0.0f64; count];
            let mut g = vec![0.0f64; count];

            f[0] = 0.5;
            e[count - 1] = 0.5;
            g[0] = 0.75 * (s[0] + s[1]);
            g[count - 1] = 0.75 * (s[count - 2] + s[count - 1]);

            for j in 1..count - 1 {
                a = (x[j + 1] - x[j - 1]) * 2.0;
                e[j] = (x[j + 1] - x[j]) / a;
                f[j] = (x[j] - x[j - 1]) / a;
                g[j] = 1.5 * s[j];
            }

            for j in 1..count {
                a = 1.0 - f[j - 1] * e[j];
                if j != count - 1 {
                    f[j] /= a;
                }
                g[j] = (g[j] - g[j - 1] * e[j]) / a;
            }

            for j in (0..count - 1).rev() {
                g[j] -= f[j] * g[j + 1];
            }

            s = g;
        }

        Ok(Self { x, y, s })
    }

    /// `IsIdentity`: exactly the (0,0)-(1,1) two-knot curve.
    pub(crate) fn is_identity(&self) -> bool {
        self.x.len() == 2
            && self.x[0] == 0.0
            && self.x[1] == 1.0
            && self.y[0] == 0.0
            && self.y[1] == 1.0
    }

    /// `Evaluate` without slope extension: clamps to the outer knot values
    /// outside `[x[0], x[last]]` (the SDR render path; the slope extension is
    /// only used for HDR overrange).
    pub(crate) fn evaluate(&self, x: f64) -> f64 {
        let count = self.x.len();

        if x <= self.x[0] {
            return self.y[0];
        }
        if x >= self.x[count - 1] {
            return self.y[count - 1];
        }

        // Binary search for the segment index j: x[j-1] < x <= x[j].
        let mut lower = 1usize;
        let mut upper = count - 1;
        while upper > lower {
            let mid = (lower + upper) >> 1;
            if x == self.x[mid] {
                return self.y[mid];
            }
            if x > self.x[mid] {
                lower = mid + 1;
            } else {
                upper = mid;
            }
        }

        self.evaluate_segment(x, lower - 1)
    }

    /// `RefBaselineRGBTone` (`dng_reference.cpp:1868-1990`): the ratio-
    /// preserving application of a tone curve to a linear RGB pixel — the way
    /// `dng_render` applies `fToneCurve` (the ProfileToneCurve composite) to
    /// image data. Channels are pinned to `[0,1]`; the curve is evaluated on
    /// the max and min channels while the middle channel lerps at the same
    /// input fraction, preserving channel ratios (hue). A literal per-channel
    /// map would shift saturated hues — this is a *verified* semantic, not a
    /// stylistic choice.
    pub(crate) fn apply_rgb(&self, pixel: &mut [f32; 3]) {
        // `Pin_real32` (`Max(0, Min(x, 1))`): NaN compares false both ways and
        // pins to 1.0, exactly the SDK macro's composition.
        let pin = |x: f64| if x < 1.0 { x.max(0.0) } else { 1.0 };
        let r = pin(f64::from(pixel[0]));
        let g = pin(f64::from(pixel[1]));
        let b = pin(f64::from(pixel[2]));

        // The RGBTone macro: curve on lo + hi, mid lerped between them.
        let tone = |lo: f64, mid: f64, hi: f64| {
            let lo_out = self.evaluate(lo);
            let hi_out = self.evaluate(hi);
            let mid_out = lo_out + (hi_out - lo_out) * (mid - lo) / (hi - lo);
            (lo_out, mid_out, hi_out)
        };

        // The SDK's 7 ordering cases (verbatim case analysis).
        let out = if r >= g {
            if g > b {
                // Case 1: r >= g > b
                let (bb, gg, rr) = tone(b, g, r);
                [rr, gg, bb]
            } else if b > r {
                // Case 2: b > r >= g
                let (gg, rr, bb) = tone(g, r, b);
                [rr, gg, bb]
            } else if b > g {
                // Case 3: r >= b > g
                let (gg, bb, rr) = tone(g, b, r);
                [rr, bb, gg]
            } else {
                // Case 4: r >= g == b (covers r == g == b)
                let rr = self.evaluate(r);
                let gg = self.evaluate(g);
                [rr, gg, gg]
            }
        } else if r >= b {
            // Case 5: g > r >= b
            let (bb, rr, gg) = tone(b, r, g);
            [rr, gg, bb]
        } else if b > g {
            // Case 6: b > g > r
            let (rr, gg, bb) = tone(r, g, b);
            [rr, gg, bb]
        } else {
            // Case 7: g >= b > r
            let (rr, bb, gg) = tone(r, b, g);
            [rr, bb, gg]
        };

        *pixel = [out[0] as f32, out[1] as f32, out[2] as f32];
    }

    /// `EvaluateSplineSegment` (`dng_spline.h:21-41`): the cubic Hermite
    /// segment between knots `i` and `i + 1` with the given end slopes.
    fn evaluate_segment(&self, x: f64, i: usize) -> f64 {
        let (x0, y0, s0) = (self.x[i], self.y[i], self.s[i]);
        let (x1, y1, s1) = (self.x[i + 1], self.y[i + 1], self.s[i + 1]);
        let a = x1 - x0;
        let b = (x - x0) / a;
        let c = (x1 - x) / a;

        ((y0 * (2.0 - c + b) + (s0 * a * b)) * (c * c))
            + ((y1 * (2.0 - b + c) - (s1 * a * c)) * (b * b))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64, tol: f64) {
        assert!((a - b).abs() <= tol, "{a} != {b} within {tol}");
    }

    #[test]
    fn two_knot_identity_is_detected_and_linear() {
        let spline = DngSpline::solve(&[(0.0, 0.0), (1.0, 1.0)]).unwrap();
        assert!(spline.is_identity());
        for x in [0.0, 0.25, 0.5, 0.75, 1.0] {
            approx(spline.evaluate(x), x, 1e-12);
        }
    }

    #[test]
    fn spline_interpolates_all_knots() {
        let knots = [(0.0, 0.0), (0.25, 0.2), (0.5, 0.5), (0.75, 0.8), (1.0, 1.0)];
        let spline = DngSpline::solve(&knots).unwrap();
        for (x, y) in knots {
            approx(spline.evaluate(x), y, 1e-12);
        }
    }

    #[test]
    fn spline_is_continuous_between_knots() {
        let knots = [(0.0, 0.0), (0.3, 0.15), (0.7, 0.9), (1.0, 1.0)];
        let spline = DngSpline::solve(&knots).unwrap();
        let eps = 1e-7;
        for &x in &[0.3, 0.7] {
            let l = spline.evaluate(x - eps);
            let r = spline.evaluate(x + eps);
            approx(l, r, 1e-4);
        }
    }

    #[test]
    fn spline_extends_flat_beyond_knots() {
        let spline = DngSpline::solve(&[(0.1, 0.2), (0.9, 0.8)]).unwrap();
        approx(spline.evaluate(-0.5), 0.2, 0.0);
        approx(spline.evaluate(2.0), 0.8, 0.0);
    }

    #[test]
    fn rejects_non_increasing_knots() {
        assert!(DngSpline::solve(&[(0.0, 0.0), (0.0, 1.0)]).is_err());
        assert!(DngSpline::solve(&[(0.5, 0.0), (0.4, 1.0)]).is_err());
        assert!(DngSpline::solve(&[(0.0, 0.0)]).is_err());
    }

    #[test]
    fn monotone_curve_stays_monotone() {
        // A typical Adobe base tone curve shape; must stay non-decreasing.
        let knots = [
            (0.0, 0.0),
            (0.1, 0.08),
            (0.25, 0.20),
            (0.5, 0.55),
            (0.75, 0.85),
            (1.0, 1.0),
        ];
        let spline = DngSpline::solve(&knots).unwrap();
        let mut prev = f64::MIN;
        for i in 0..=200 {
            let y = spline.evaluate(i as f64 / 200.0);
            assert!(y >= prev - 1e-9, "not monotone at {i}: {prev} -> {y}");
            prev = y;
        }
    }
}
