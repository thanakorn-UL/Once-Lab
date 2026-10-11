# RAW Baseline Divergence Report — Canon EOS R5 CR3 (Once-Lab vs. Lightroom Classic)

Branch: `feature/raw-baseline-diagnostics` (stacked on PR #4 `feature/dcp-camera-profile`).
Method: public DNG SDK 1.7.1 source review + measured execution on the real CR3 via the
`raw_diagnostics` instrumented pipeline (`get_raw_baseline_diagnostics`,
`ONCELAB_RAW_FIXTURE` / `ONCELAB_RAW_DIAG_DIR` env-gated test).

Measured fixture: `Canon-eos-r5-raw-00023.cr3` (8352×5586 CFA, RGGB, cpp=1, 3 unique colors).

## Tier 1 — Verified correct (no action)

**Linearization / black+white normalization.** Rawler's integer path executes
`(raw − black_cfa)/(u32::MAX − black_cfa)`; Once-Lab's `whitelevel = u32::MAX` override
turns it into exactly that, and `rescale_factor = (u32::MAX − black_0)/(white_0 − black_0)`
restores approximately the SDK denominator. Measured effective scale per CFA plane vs.
the ideal `1/(white_cfa − black_cfa)`:

- planes `[511, 510, 510, 511]`, white `14008`
- denominators `[13497, 13498, 13498, 13497]`
- **max |scale_deviation| = 5.5e-9 absolute** — i.e. ≈**0.0074% relative error** on the
  green planes (scale ~7.4e-5). This is NOT pure floating-point rounding: the rescale
  applies the plane-0 denominator `(white_0 − black_0)` to every channel, while the SDK
  uses each plane's own `(white_cfa − black_cfa)`. The dominant term is the
  `(white_0 − black_0)/(white_cfa − black_cfa)` ratio (≈0.0074% for ±1-level plane
  differences); the residual `(u32::MAX − b_i)` mismatch is ~2e-10. It is a small,
  deterministic approximation to the SDK normalization — correct in structure
  (single subtraction, single scaling, no linearization LUT), inexact in the
  per-plane denominators by ~1 part in 13,500.

**Stage counts.** The Adobe path removes Rawler's `Calibrate` step (which would
double-apply WB) and folds WB into the camera transform — matches the SDK's
single-subtraction/single-scaling contract.

**Demosaic/crop ordering** matches `dng_1st_function` → Rescale → Crop.

## Tier 2 — Verified divergence, fixed in this branch

**Sub-black sensor values were destroyed before demosaic.** Rawler's
`correct_blacklevel_cfa` executes `clip(raw − black)` — hard zero-clamp per CFA plane
*before* demosaic. The SDK's stage3 keeps signed values (no `forceClipBlackLevel`).

- Measured on the fixture: **237,837 mosaic samples (0.51%)** sit below their plane's
  black level (mosaic min 493 vs. blacks ~511).
- Fix: for the single-sample 3-color CFA case, `develop_intermediate_signed_cfa`
  replicates Rawler's own `Rescale → Demosaic(active area) → CropDefault` steps with
  a **signed** black correction (`correct_blacklevel_cfa_signed` — same formula, no
  clip). This is NOT a constant-offset trick: the demosaic input is exactly the
  signed-normalized mosaic the SDK would produce, in the same magnitude domain —
  so PPG's gradient-branch decisions and `hue_transit` ties see identical values.
  (An earlier pedestal-shift design was rejected: shifting the input domain perturbs
  near-tie branch decisions through f32 rounding — measured up to 3.3e-3 error on an
  adversarial synthetic — and required an "approximate equivariance" argument that
  does not survive Rawler's gradient-adaptive kernels.)
- Guarded: Adobe-comparison path only; 4-color CFAs and non-CFA inputs keep Rawler's
  clipped path.
- Verified end-to-end: `correct_blacklevel_cfa_signed` keeps negatives on below-black
  inputs, and real-PPG demosaic of the signed mosaic differs from the clipped mosaic
  only inside the stencil neighborhoods of sub-black samples (measured max |Δ| =
  7.2e-9 in the u32::MAX-normalized domain on the synthetic fixture — i.e. real but
  visually nil correction, as predicted by the 0.5% sparse-sample statistics).

## Tier 3 — Verified divergence, implemented as an *opt-in*

**Missing baseline tone stage.** The SDK substitutes `dng_tone_curve_acr3_default`
(a baked 1025-sample table, `dng_render.cpp:131-410`) when a profile has no
`ProfileToneCurve`, applied via `RefBaselineRGBTone` (ratio-preserving, not per-channel).
Once-Lab previously ran identity here.

- Implemented: `SampledToneCurve::acr3_default()` (verbatim table + SDK linear
  interpolation incl. its index-pinned extrapolation), unified `BaselineToneCurve`
  trait so DCP splines and the sampled table share the verbatim 7-case `apply_rgb`.
- Also implemented: SDK render-default exposure ramp (`Shadows=5`, `ShadowScale=1`,
  EV 0 → black=0.005 quadratic toe) on the fallback path under the same flag; DCP
  profiles continue to carry their own ramp (BaselineExposureOffset + black render).
- **Opt-in only** (`adobe_dng_baseline_tone` setting + `ONCELAB_ADOBE_DNG_TONE=1` +
  Settings → Processing → "DNG Baseline Tone Curve" toggle). Default OFF preserves the
  established baseline.

**Measured effect on the fixture (Rawler-fallback path):**

| Stage (final linear sRGB) | luma mean | luma p50 | luma p99 | luma max |
|---|---|---|---|---|
| identity baseline | 0.0684 | 0.0380 | 0.2992 | 0.798 |
| acr3 + Shadows=5 ramp | 0.1163 | 0.0444 | 0.5865 | 0.940 |

The ACR3 experiment demonstrates a **large modeled tone effect** on Once-Lab's
own baseline — the identity tone leaves the working buffer ~1.7× darker than the
SDK default. Whether it is the *dominant cause* of the Once-vs-Lightroom
divergence is **not established**: that requires the owner's controlled
A0/A100 TIFF comparison. The acr3 table is the documented SDK fallback,
**not** PV2012, and is presented only as a selectable baseline for those runs.

## Tier 4 — Cannot be reproduced exactly (documented, not fabricated)

1. **PV2012 adaptive tone stack** (`cr_stage_tone_map*` private Lightroom modules).
   `acr3_default` is the SDK public fallback — deliberately not a claim of parity.
2. **Invariate-model highlight rendering.** The fallback path applies WB as a scalar
   multiply + inverted color matrix (ColorMatrix2 21[18]); the SDK's `SetWhiteXY` /
   `ForwardMatrix` invariate path needs the camera's ForwardMatrix data. Owner cannot
   distribute Adobe Standard.dcp, and inventing one is out of bounds — the fallback
   stays WB-multiply (recorded verbatim in `fallbackTransform` diagnostics).
3. **PV2012 exposure baseline.** Lightroom applies private brightness steps beyond the
   SDK ramp — unimplemented by design.
4. **Highlight-negative handling at output.** Signed out-of-gamut excursions are
   preserved through the ProPhoto→sRGB hop and pinned at the output encode — the
   final sRGB working buffer shows 1,468 B-negatives / 13 >1.0 before encode
   (identity run), matching the SDK's output-stage pinning contract.

## Instrumentation delivered

- `RawPipelineDiagnostics` JSON (`schema: oncelab-raw-baseline-diagnostics/v1`):
  decoder provenance (per-CFA blacks, white, WB, active/crop, masked areas, color
  matrices, per-plane raw mosaic stats with below-black/saturated counts),
  linearization proof (effective vs. SDK scale per plane), exposure/tone report
  (ramp label, BEO, selected tone curve), per-stage 64-bin histograms + percentiles +
  luminance + negative/over-range counts, fallback transform matrix, output-stage
  description. Tauri command: `get_raw_baseline_diagnostics`.
- Optional stage dumps via `ONCELAB_RAW_DIAG_DIR`: a verbatim signed
  `stage-*.f32` numerical dump (+`.json` metadata: dtype, layout, source color
  space, subsample factor) and a `*-preview.png` display image that is
  explicitly sRGB-encoded and clamped — the preview is not a pixel dump.
- Env-gated CR3 test asserts: correct provenance, `maxScaleDeviation < 1e-7`,
  `singleBlackSubtraction`, `subBlackPreserved`, stage presence, tone selection.

## Preserved invariants

- Default (`identity`) path is pixel-identical to PR #4 except the signed sub-black
  preservation (Tier 2).
- Au RGBTable evaluation untouched; golden oracle tests all pass (411 lib tests).
- Preview/display U8 path untouched; TIFF export still reads back f16.
- No Adobe assets extracted or committed; no Lightroom-parity claim.
