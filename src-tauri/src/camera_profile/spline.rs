//! Port of the public DNG SDK `dng_spline_solver` (`dng_spline.cpp`), used to
//! evaluate a camera profile's `ProfileToneCurve` (tag 50940): a C0/C1/C2
//! continuous cubic spline through the authored `(x, y)` knots with zero
//! second derivative at the end points.
//!
//! `dng_render` concatenates this function with the exposure-tone function and
//! samples it into a `dng_1d_table`; we evaluate the solved spline directly,
//! which matches the sampled table to well below output quantization.

/// A baseline tone stage as `dng_render` uses it: a `dng_1d_function`
/// evaluated through `RefBaselineRGBTone` (ratio-preserving application).
/// Implemented by [`DngSpline`] (profile-authored `ProfileToneCurve`) and
/// [`SampledToneCurve`] (the SDK's baked `dng_tone_curve_acr3_default`).
pub(crate) trait BaselineToneCurve {
    /// `Evaluate` on the linear input value.
    fn evaluate(&self, x: f64) -> f64;
    /// Diagnostic label for reports ("identity", "acr3 default", ...).
    fn label(&self) -> &'static str;

    /// `RefBaselineRGBTone` (`dng_reference.cpp:1868-1990`): the ratio-
    /// preserving application of a tone curve to a linear RGB pixel — the way
    /// `dng_render` applies `fToneCurve` (the ProfileToneCurve composite) to
    /// image data. Channels are pinned to `[0,1]`; the curve is evaluated on
    /// the max and min channels while the middle channel lerps at the same
    /// input fraction, preserving channel ratios (hue). A literal per-channel
    /// map would shift saturated hues — this is a *verified* semantic, not a
    /// stylistic choice.
    fn apply_rgb(&self, pixel: &mut [f32; 3]) {
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
}

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

impl BaselineToneCurve for DngSpline {
    fn evaluate(&self, x: f64) -> f64 {
        DngSpline::evaluate(self, x)
    }

    fn label(&self) -> &'static str {
        "dng ProfileToneCurve (spline)"
    }
}

/// `dng_tone_curve_acr3_default` (`dng_render.cpp:131-410`): the baseline
/// tone curve the DNG SDK substitutes when a profile carries no
/// `ProfileToneCurve` — a baked 1025-entry sampled table evaluated by
/// linear interpolation over `[0,1]`. This is the *documented* SDK
/// fallback; it is NOT a reproduction of Lightroom's private PV2012
/// adaptive tone stack (`cr_stage_tone_map*`). Selectable via the
/// `adobe_dng_baseline_tone` setting; default remains identity.
pub(crate) struct SampledToneCurve {
    table: &'static [f32],
    label: &'static str,
}

impl SampledToneCurve {
    /// The SDK's baked default curve (`dng_tone_curve_acr3_default::Get`).
    pub(crate) fn acr3_default() -> Self {
        Self {
            table: &ACR3_DEFAULT_TABLE,
            label: "dng_tone_curve_acr3_default (1025-pt sampled)",
        }
    }
}

impl BaselineToneCurve for SampledToneCurve {
    /// `Evaluate` (`dng_render.cpp:391-407`): `y = x * (N-1)`,
    /// `index = pin(0, y, N-2)`, linear interpolation.
    fn evaluate(&self, x: f64) -> f64 {
        let n = self.table.len();
        let y = (x as f32) * (n - 1) as f32;
        let index = (y as i32).clamp(0, n as i32 - 2) as usize;
        let fract = y - index as f32;
        (self.table[index] as f64) * (1.0 - fract as f64)
            + (self.table[index + 1] as f64) * fract as f64
    }

    fn label(&self) -> &'static str {
        self.label
    }
}

/// The verbatim `kTable` from `dng_tone_curve_acr3_default::Evaluate`.
#[allow(clippy::approx_constant)] // data literals, not approximations
static ACR3_DEFAULT_TABLE: [f32; 1025] = [
    0.00000, 0.00078, 0.00160, 0.00242, 0.00314, 0.00385, 0.00460, 0.00539, 0.00623, 0.00712,
    0.00806, 0.00906, 0.01012, 0.01122, 0.01238, 0.01359, 0.01485, 0.01616, 0.01751, 0.01890,
    0.02033, 0.02180, 0.02331, 0.02485, 0.02643, 0.02804, 0.02967, 0.03134, 0.03303, 0.03475,
    0.03648, 0.03824, 0.04002, 0.04181, 0.04362, 0.04545, 0.04730, 0.04916, 0.05103, 0.05292,
    0.05483, 0.05675, 0.05868, 0.06063, 0.06259, 0.06457, 0.06655, 0.06856, 0.07057, 0.07259,
    0.07463, 0.07668, 0.07874, 0.08081, 0.08290, 0.08499, 0.08710, 0.08921, 0.09134, 0.09348,
    0.09563, 0.09779, 0.09996, 0.10214, 0.10433, 0.10652, 0.10873, 0.11095, 0.11318, 0.11541,
    0.11766, 0.11991, 0.12218, 0.12445, 0.12673, 0.12902, 0.13132, 0.13363, 0.13595, 0.13827,
    0.14061, 0.14295, 0.14530, 0.14765, 0.15002, 0.15239, 0.15477, 0.15716, 0.15956, 0.16197,
    0.16438, 0.16680, 0.16923, 0.17166, 0.17410, 0.17655, 0.17901, 0.18148, 0.18395, 0.18643,
    0.18891, 0.19141, 0.19391, 0.19641, 0.19893, 0.20145, 0.20398, 0.20651, 0.20905, 0.21160,
    0.21416, 0.21672, 0.21929, 0.22185, 0.22440, 0.22696, 0.22950, 0.23204, 0.23458, 0.23711,
    0.23963, 0.24215, 0.24466, 0.24717, 0.24967, 0.25216, 0.25465, 0.25713, 0.25961, 0.26208,
    0.26454, 0.26700, 0.26945, 0.27189, 0.27433, 0.27676, 0.27918, 0.28160, 0.28401, 0.28641,
    0.28881, 0.29120, 0.29358, 0.29596, 0.29833, 0.30069, 0.30305, 0.30540, 0.30774, 0.31008,
    0.31241, 0.31473, 0.31704, 0.31935, 0.32165, 0.32395, 0.32623, 0.32851, 0.33079, 0.33305,
    0.33531, 0.33756, 0.33981, 0.34205, 0.34428, 0.34650, 0.34872, 0.35093, 0.35313, 0.35532,
    0.35751, 0.35969, 0.36187, 0.36404, 0.36620, 0.36835, 0.37050, 0.37264, 0.37477, 0.37689,
    0.37901, 0.38112, 0.38323, 0.38533, 0.38742, 0.38950, 0.39158, 0.39365, 0.39571, 0.39777,
    0.39982, 0.40186, 0.40389, 0.40592, 0.40794, 0.40996, 0.41197, 0.41397, 0.41596, 0.41795,
    0.41993, 0.42191, 0.42388, 0.42584, 0.42779, 0.42974, 0.43168, 0.43362, 0.43554, 0.43747,
    0.43938, 0.44129, 0.44319, 0.44509, 0.44698, 0.44886, 0.45073, 0.45260, 0.45447, 0.45632,
    0.45817, 0.46002, 0.46186, 0.46369, 0.46551, 0.46733, 0.46914, 0.47095, 0.47275, 0.47454,
    0.47633, 0.47811, 0.47989, 0.48166, 0.48342, 0.48518, 0.48693, 0.48867, 0.49041, 0.49214,
    0.49387, 0.49559, 0.49730, 0.49901, 0.50072, 0.50241, 0.50410, 0.50579, 0.50747, 0.50914,
    0.51081, 0.51247, 0.51413, 0.51578, 0.51742, 0.51906, 0.52069, 0.52232, 0.52394, 0.52556,
    0.52717, 0.52878, 0.53038, 0.53197, 0.53356, 0.53514, 0.53672, 0.53829, 0.53986, 0.54142,
    0.54297, 0.54452, 0.54607, 0.54761, 0.54914, 0.55067, 0.55220, 0.55371, 0.55523, 0.55673,
    0.55824, 0.55973, 0.56123, 0.56271, 0.56420, 0.56567, 0.56715, 0.56861, 0.57007, 0.57153,
    0.57298, 0.57443, 0.57587, 0.57731, 0.57874, 0.58017, 0.58159, 0.58301, 0.58443, 0.58583,
    0.58724, 0.58864, 0.59003, 0.59142, 0.59281, 0.59419, 0.59556, 0.59694, 0.59830, 0.59966,
    0.60102, 0.60238, 0.60373, 0.60507, 0.60641, 0.60775, 0.60908, 0.61040, 0.61173, 0.61305,
    0.61436, 0.61567, 0.61698, 0.61828, 0.61957, 0.62087, 0.62216, 0.62344, 0.62472, 0.62600,
    0.62727, 0.62854, 0.62980, 0.63106, 0.63232, 0.63357, 0.63482, 0.63606, 0.63730, 0.63854,
    0.63977, 0.64100, 0.64222, 0.64344, 0.64466, 0.64587, 0.64708, 0.64829, 0.64949, 0.65069,
    0.65188, 0.65307, 0.65426, 0.65544, 0.65662, 0.65779, 0.65897, 0.66013, 0.66130, 0.66246,
    0.66362, 0.66477, 0.66592, 0.66707, 0.66821, 0.66935, 0.67048, 0.67162, 0.67275, 0.67387,
    0.67499, 0.67611, 0.67723, 0.67834, 0.67945, 0.68055, 0.68165, 0.68275, 0.68385, 0.68494,
    0.68603, 0.68711, 0.68819, 0.68927, 0.69035, 0.69142, 0.69249, 0.69355, 0.69461, 0.69567,
    0.69673, 0.69778, 0.69883, 0.69988, 0.70092, 0.70196, 0.70300, 0.70403, 0.70506, 0.70609,
    0.70711, 0.70813, 0.70915, 0.71017, 0.71118, 0.71219, 0.71319, 0.71420, 0.71520, 0.71620,
    0.71719, 0.71818, 0.71917, 0.72016, 0.72114, 0.72212, 0.72309, 0.72407, 0.72504, 0.72601,
    0.72697, 0.72794, 0.72890, 0.72985, 0.73081, 0.73176, 0.73271, 0.73365, 0.73460, 0.73554,
    0.73647, 0.73741, 0.73834, 0.73927, 0.74020, 0.74112, 0.74204, 0.74296, 0.74388, 0.74479,
    0.74570, 0.74661, 0.74751, 0.74842, 0.74932, 0.75021, 0.75111, 0.75200, 0.75289, 0.75378,
    0.75466, 0.75555, 0.75643, 0.75730, 0.75818, 0.75905, 0.75992, 0.76079, 0.76165, 0.76251,
    0.76337, 0.76423, 0.76508, 0.76594, 0.76679, 0.76763, 0.76848, 0.76932, 0.77016, 0.77100,
    0.77183, 0.77267, 0.77350, 0.77432, 0.77515, 0.77597, 0.77680, 0.77761, 0.77843, 0.77924,
    0.78006, 0.78087, 0.78167, 0.78248, 0.78328, 0.78408, 0.78488, 0.78568, 0.78647, 0.78726,
    0.78805, 0.78884, 0.78962, 0.79040, 0.79118, 0.79196, 0.79274, 0.79351, 0.79428, 0.79505,
    0.79582, 0.79658, 0.79735, 0.79811, 0.79887, 0.79962, 0.80038, 0.80113, 0.80188, 0.80263,
    0.80337, 0.80412, 0.80486, 0.80560, 0.80634, 0.80707, 0.80780, 0.80854, 0.80926, 0.80999,
    0.81072, 0.81144, 0.81216, 0.81288, 0.81360, 0.81431, 0.81503, 0.81574, 0.81645, 0.81715,
    0.81786, 0.81856, 0.81926, 0.81996, 0.82066, 0.82135, 0.82205, 0.82274, 0.82343, 0.82412,
    0.82480, 0.82549, 0.82617, 0.82685, 0.82753, 0.82820, 0.82888, 0.82955, 0.83022, 0.83089,
    0.83155, 0.83222, 0.83288, 0.83354, 0.83420, 0.83486, 0.83552, 0.83617, 0.83682, 0.83747,
    0.83812, 0.83877, 0.83941, 0.84005, 0.84069, 0.84133, 0.84197, 0.84261, 0.84324, 0.84387,
    0.84450, 0.84513, 0.84576, 0.84639, 0.84701, 0.84763, 0.84825, 0.84887, 0.84949, 0.85010,
    0.85071, 0.85132, 0.85193, 0.85254, 0.85315, 0.85375, 0.85436, 0.85496, 0.85556, 0.85615,
    0.85675, 0.85735, 0.85794, 0.85853, 0.85912, 0.85971, 0.86029, 0.86088, 0.86146, 0.86204,
    0.86262, 0.86320, 0.86378, 0.86435, 0.86493, 0.86550, 0.86607, 0.86664, 0.86720, 0.86777,
    0.86833, 0.86889, 0.86945, 0.87001, 0.87057, 0.87113, 0.87168, 0.87223, 0.87278, 0.87333,
    0.87388, 0.87443, 0.87497, 0.87552, 0.87606, 0.87660, 0.87714, 0.87768, 0.87821, 0.87875,
    0.87928, 0.87981, 0.88034, 0.88087, 0.88140, 0.88192, 0.88244, 0.88297, 0.88349, 0.88401,
    0.88453, 0.88504, 0.88556, 0.88607, 0.88658, 0.88709, 0.88760, 0.88811, 0.88862, 0.88912,
    0.88963, 0.89013, 0.89063, 0.89113, 0.89163, 0.89212, 0.89262, 0.89311, 0.89360, 0.89409,
    0.89458, 0.89507, 0.89556, 0.89604, 0.89653, 0.89701, 0.89749, 0.89797, 0.89845, 0.89892,
    0.89940, 0.89987, 0.90035, 0.90082, 0.90129, 0.90176, 0.90222, 0.90269, 0.90316, 0.90362,
    0.90408, 0.90454, 0.90500, 0.90546, 0.90592, 0.90637, 0.90683, 0.90728, 0.90773, 0.90818,
    0.90863, 0.90908, 0.90952, 0.90997, 0.91041, 0.91085, 0.91130, 0.91173, 0.91217, 0.91261,
    0.91305, 0.91348, 0.91392, 0.91435, 0.91478, 0.91521, 0.91564, 0.91606, 0.91649, 0.91691,
    0.91734, 0.91776, 0.91818, 0.91860, 0.91902, 0.91944, 0.91985, 0.92027, 0.92068, 0.92109,
    0.92150, 0.92191, 0.92232, 0.92273, 0.92314, 0.92354, 0.92395, 0.92435, 0.92475, 0.92515,
    0.92555, 0.92595, 0.92634, 0.92674, 0.92713, 0.92753, 0.92792, 0.92831, 0.92870, 0.92909,
    0.92947, 0.92986, 0.93025, 0.93063, 0.93101, 0.93139, 0.93177, 0.93215, 0.93253, 0.93291,
    0.93328, 0.93366, 0.93403, 0.93440, 0.93478, 0.93515, 0.93551, 0.93588, 0.93625, 0.93661,
    0.93698, 0.93734, 0.93770, 0.93807, 0.93843, 0.93878, 0.93914, 0.93950, 0.93986, 0.94021,
    0.94056, 0.94092, 0.94127, 0.94162, 0.94197, 0.94231, 0.94266, 0.94301, 0.94335, 0.94369,
    0.94404, 0.94438, 0.94472, 0.94506, 0.94540, 0.94573, 0.94607, 0.94641, 0.94674, 0.94707,
    0.94740, 0.94774, 0.94807, 0.94839, 0.94872, 0.94905, 0.94937, 0.94970, 0.95002, 0.95035,
    0.95067, 0.95099, 0.95131, 0.95163, 0.95194, 0.95226, 0.95257, 0.95289, 0.95320, 0.95351,
    0.95383, 0.95414, 0.95445, 0.95475, 0.95506, 0.95537, 0.95567, 0.95598, 0.95628, 0.95658,
    0.95688, 0.95718, 0.95748, 0.95778, 0.95808, 0.95838, 0.95867, 0.95897, 0.95926, 0.95955,
    0.95984, 0.96013, 0.96042, 0.96071, 0.96100, 0.96129, 0.96157, 0.96186, 0.96214, 0.96242,
    0.96271, 0.96299, 0.96327, 0.96355, 0.96382, 0.96410, 0.96438, 0.96465, 0.96493, 0.96520,
    0.96547, 0.96574, 0.96602, 0.96629, 0.96655, 0.96682, 0.96709, 0.96735, 0.96762, 0.96788,
    0.96815, 0.96841, 0.96867, 0.96893, 0.96919, 0.96945, 0.96971, 0.96996, 0.97022, 0.97047,
    0.97073, 0.97098, 0.97123, 0.97149, 0.97174, 0.97199, 0.97223, 0.97248, 0.97273, 0.97297,
    0.97322, 0.97346, 0.97371, 0.97395, 0.97419, 0.97443, 0.97467, 0.97491, 0.97515, 0.97539,
    0.97562, 0.97586, 0.97609, 0.97633, 0.97656, 0.97679, 0.97702, 0.97725, 0.97748, 0.97771,
    0.97794, 0.97817, 0.97839, 0.97862, 0.97884, 0.97907, 0.97929, 0.97951, 0.97973, 0.97995,
    0.98017, 0.98039, 0.98061, 0.98082, 0.98104, 0.98125, 0.98147, 0.98168, 0.98189, 0.98211,
    0.98232, 0.98253, 0.98274, 0.98295, 0.98315, 0.98336, 0.98357, 0.98377, 0.98398, 0.98418,
    0.98438, 0.98458, 0.98478, 0.98498, 0.98518, 0.98538, 0.98558, 0.98578, 0.98597, 0.98617,
    0.98636, 0.98656, 0.98675, 0.98694, 0.98714, 0.98733, 0.98752, 0.98771, 0.98789, 0.98808,
    0.98827, 0.98845, 0.98864, 0.98882, 0.98901, 0.98919, 0.98937, 0.98955, 0.98973, 0.98991,
    0.99009, 0.99027, 0.99045, 0.99063, 0.99080, 0.99098, 0.99115, 0.99133, 0.99150, 0.99167,
    0.99184, 0.99201, 0.99218, 0.99235, 0.99252, 0.99269, 0.99285, 0.99302, 0.99319, 0.99335,
    0.99351, 0.99368, 0.99384, 0.99400, 0.99416, 0.99432, 0.99448, 0.99464, 0.99480, 0.99495,
    0.99511, 0.99527, 0.99542, 0.99558, 0.99573, 0.99588, 0.99603, 0.99619, 0.99634, 0.99649,
    0.99664, 0.99678, 0.99693, 0.99708, 0.99722, 0.99737, 0.99751, 0.99766, 0.99780, 0.99794,
    0.99809, 0.99823, 0.99837, 0.99851, 0.99865, 0.99879, 0.99892, 0.99906, 0.99920, 0.99933,
    0.99947, 0.99960, 0.99974, 0.99987, 1.00000,
];

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

    /// The ACR3 default is the verbatim SDK table: monotone, endpoints
    /// exact, and spot values matching `dng_render.cpp`'s `kTable`.
    #[test]
    fn acr3_default_matches_sdk_table_shape() {
        let curve = SampledToneCurve::acr3_default();
        approx(curve.evaluate(0.0), 0.0, 1e-7);
        approx(curve.evaluate(1.0), 1.0, 1e-7);
        // Exact nodes: evaluate(i/1024) == kTable[i]. Spot values below
        // are copied verbatim from `dng_render.cpp`'s `kTable`.
        for (i, want) in [
            (64usize, 0.10433f64),
            (128, 0.25961),
            (256, 0.52069),
            (512, 0.80486),
            (768, 0.93986),
            (896, 0.97702),
        ] {
            approx(curve.evaluate(i as f64 / 1024.0), want, 1e-4);
        }
        let mut prev = -1.0f64;
        for i in 0..=1024 {
            let y = curve.evaluate(i as f64 / 1024.0);
            assert!(y >= prev - 1e-9, "acr3 table not monotone at {i}");
            prev = y;
        }
        // Verbatim SDK behavior for out-of-domain x: Pin_int32 clamps the
        // INDEX only, the fraction still extrapolates (the caller —
        // RefBaselineRGBTone — pins inputs to [0,1] first).
        // x=-0.25 -> y=-256 -> index 0, fract -256 -> -256*kTable[1].
        approx(curve.evaluate(-0.25), -256.0 * 0.00078, 1e-6);
        // x=1.5 -> y=1536 -> index 1023, fract 513 -> lerp past the end.
        approx(
            curve.evaluate(1.5),
            0.99987 * (1.0 - 513.0) + 1.0 * 513.0,
            1e-4,
        );
    }

    /// `apply_rgb` on the sampled curve keeps the mid channel's relative
    /// position between the curve outputs at lo and hi (ratio-preserving).
    #[test]
    fn acr3_apply_rgb_preserves_channel_ratios() {
        let curve = SampledToneCurve::acr3_default();
        let mut pixel = [0.3f32, 0.6, 0.15];
        curve.apply_rgb(&mut pixel);
        let lo = curve.evaluate(0.15);
        let hi = curve.evaluate(0.6);
        let expect_r = lo + (hi - lo) * (0.3 - 0.15) / (0.6 - 0.15);
        approx(pixel[0] as f64, expect_r, 1e-6);
        approx(pixel[1] as f64, hi, 1e-6);
        approx(pixel[2] as f64, lo, 1e-6);
        // The curve darkens mids but never inverts ordering.
        assert!(pixel[2] <= pixel[0] && pixel[0] <= pixel[1]);
    }

    /// A neutral pixel stays neutral: equal channels share lo==hi, so the
    /// lerp degenerates to `evaluate(x)` for all three.
    #[test]
    fn acr3_neutral_stays_neutral() {
        let curve = SampledToneCurve::acr3_default();
        let mut pixel = [0.42f32, 0.42, 0.42];
        curve.apply_rgb(&mut pixel);
        let want = curve.evaluate(0.42) as f32;
        assert!((pixel[0] - want).abs() < 1e-6);
        assert!((pixel[0] - pixel[1]).abs() < 1e-7);
        assert!((pixel[1] - pixel[2]).abs() < 1e-7);
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
