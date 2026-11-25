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
- **Developer-only experiment selector** — four modes via
  `ONCELAB_ADOBE_DNG_TONE_MODE` (or the `adobeDngBaselineMode` settings key;
  intentionally no permanent Settings UI):
  `identity` (default) · `acr3` (tone curve only) · `shadow-toe` (Shadows=5
  ramp only) · `acr3+toe` (both). Mode is recorded in the diagnostics'
  `exposureTone.baselineMode` field for reproducibility.

**Measured effect on the fixture (Rawler-fallback path, final linear sRGB working buffer):**

| Mode | luma mean | luma p50 | luma p99 | luma max |
|---|---|---|---|---|
| `identity` (default) | 0.0684 | 0.0380 | 0.2992 | 0.798 |
| `shadow-toe` | 0.0637 | 0.0331 | 0.2956 | — |
| `acr3` | 0.1247 | 0.0534 | 0.5916 | — |
| `acr3+toe` | 0.1162 | 0.0444 | 0.5865 | 0.940 |

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

---

## RAW 00009 / Task A — per-pixel kill-chain for Once B=0 vs Lightroom B>0

Fixture: `Canon-eos-r5-raw-00009.cr3` (8192×5464 export). Mode: `acr3` baseline +
`Au ⛏️.xmp` at 0% (`ProfileRenderMode::AdobeSdk`, `adobe_comparison_mode`).
Mask: 273,792 pixels where the Once F16 TIFF has `B16 == 0` and
`Lr_Au 0%.tif` has `B16 > 0` (LR masked B mean 8,567/65535). Region: y 3530–5463,
full width — saturated warm tones.

Method: `raw_processing::round3_probe::trace_zero_blue_kill_chain` replicates the
upstream RAW path, applies the **real** profile evaluator, snapshots 7 stages per
masked pixel, and proves bit-identical output vs `develop_raw_image_with_profile`
over all 44.7M pixels (max diff 0.0). `xmp_profile::AdobeStageProbe` extracts the
production stage functions — the trace is the executed math, not a model.

### Measured stage values (B channel, 273,792 pixels)

| Stage | B < 0 | B == 0 | min | mean | Comment |
|---|---:|---:|---:|---:|---|
| camera RGB (post-demosaic/rescale) | 36 | 0 | −0.00298 | +0.02018 | signed sub-black/demosaic excursions, legit |
| ProPhoto (post-calibration) | 598 | 0 | −0.02324 | +0.02272 | +562 new negatives — normal wide-gamut excursions of saturated warm colors |
| post-LookTable | 598 | 0 | −0.02324 | +0.02272 | Au has no LookTable — pass-through |
| post-ACR3 tone | 0 | 598 | 0.0 | +0.02623 | **entry pin `[0,1]` zeroes all 598 negatives; zero positive B was killed by the curve** |
| post-Au RGBTable (0%) | 0 | 0 | +0.00072 | +0.03234 | NOT a no-op: gamut-clip table rewrites 271,234 px (99.1%), parks clipped colors on the sRGB surface, lifts ProPhoto B (mean Δ +6.1e-3) |
| sRGB pre-clamp | 58,506 | 141,890 | −1.5e-8 | ~0 | post-table conversion lands the parked colors *on* the `b=0` plane — negatives are ε-deep, not −0.083 (that figure was the counterfactual direct conversion's depth, see below) |
| working-buffer out | 0 | 200,396 | 0.0 | ~0 | 73,396 px survive with 0 < B ≤ 1.5e-4 |
| TIFF u16 | — | 273,792 | 0 | 0 | all dead |

### Attribution — disjoint, actual stages (measured, not hypothetical)

The masked 273,792 px partition into mutually exclusive sets by the **first
executed stage where B became ≤ 0** (or, if never ≤ 0, where it died):

| set | count | mechanism |
|---|---:|---|
| **S1 — ACR3 entry pin** | **598** | signed negative ProPhoto B (36 already negative in camera space) pinned at the `apply_rgb` input clamp (`camera_profile/spline.rs`, verbatim `RefBaselineRGBTone`). 437 stay ≤0 through the table+conversion; **161 are resurrected to positive** by the RGBTable (table lookups are not channel-monotone) but die at the u16 encode anyway |
| **S2 — post-Au gamut boundary pin** | **199,959** | post-RGBTable ProPhoto→sRGB conversion yields B ≤ 0 (58,506 negative, 141,890 exactly on the `b=0` plane) → the SDR output `clamp(0,1)` kills them |
| **S3 — encode floor** | **73,235** | survive every stage positive (0 < B ≤ 1.5e-4) but encode to u16 0 — see dither decomposition below |

598 + 199,959 + 73,235 = **273,792** ✓ disjoint. (The earlier
"200,396 working-buffer zeros" = S1-stayed-zero 437 + S2 199,959;
"73,396 buffer-positive dead" = 161 resurrected + 73,235.)

**Counterfactual (labeled, not actual):** converting post-tone ProPhoto→sRGB
*without* the Au table and predicting u16 yields B=0 for **271,222** of the
same mask — the Au table does not cause the loss; it moves the pin to its
own sRGB-primaries boundary and parks ~141.9k colors *on* the `b=0` plane,
which is an equivalent outcome. The earlier "270,607 hypothetical
out-of-sRGB" number refers to this counterfactual population — overlapping,
not an independent causal category.

### F16 / dither / u16 decomposition (direct no-dither control)

Re-encoding the traced `out` buffer to u16 *without* dither
(`round(sRGB(out.B)·65535)`) and comparing against the real TIFF:

- **70,665 px** of the 73,235 encode-floor deaths have predicted u16 = 0 —
  they die at ≤½ LSB **with or without dither** (f16 precision and the
  dither term are irrelevant for these).
- **2,570 px** would have encoded u16 1–47 without dither and are killed
  solely by the position-dependent biased dither (≈ −0.30/255 mean over
  the masked region, deterministic per pixel — a sparse re-render
  reproduced all 273,792 TIFF pixels bit-exactly).
- The remaining 161 (S1-resurrected) distribute the same way.

### A0 no-profile control (measured, same RAW/mode/coords)

`acr3` baseline, `xmp_profile_path=None` vs `Au ⛏️.xmp` at 0%:

- B=0 masks **bit-identical** between the two exports (282,630 px each);
  the FP-vs-LR set is identical (273,792).
- Only 215,082 px (0.48%) differ at all — max |Δ| 32/65535 (~½ u8-LSB),
  mean |Δ| on changed px 4–10 u16; 87% of changed pixels are the B channel.
- Conclusion: Au at effective amount 0 **does not change clipping or the
  zero-blue outcome**; its gamut-clip stage writes values the SDR pins
  would have produced anyway (max sub-LSB color shift elsewhere).

### Secondary finding — biased output dither

On a constant-input F16 render, residual vs CPU-simulated encode is
**−77 u16 mean at the masked coordinates** but −14 u16 globally — the
position-dependent `dither(id.xy)` term is biased ≈ −0.30/255 over this region
(tile-local hash coords, args ~1.2e4–2e5). It is deterministic per pixel (a
sparse re-render of the traced buffer reproduced all 273,792 TIFF pixels
bit-exactly) and pushed 2,570 pixels that would have encoded u16 2–119 to 0.
Small (~0.12% FS) but systematic — worth a separate look at the hash's
large-argument behavior.

### Cross-mode B=0 counts (same mask definition vs LR)

| baseline mode | Once B16=0 total | ∩ LR B>0 |
|---|---:|---:|
| identity | 26,739 | 21,899 |
| acr3 | 282,630 | 273,792 |
| shadow-toe | 88,338 | 74,831 |
| acr3+toe | 653,572 | 639,122 |

### Conclusion

The masked pixels' color is **not corrupted — it is clipped** (CODE-VERIFIED +
MEASURED). The dominant mechanism is genuinely out-of-sRGB-gamut content
meeting hard per-channel `[0,1]` pins at the ProPhoto→sRGB boundary,
preceded by the ACR3 entry pin for the 598 negative-ProPhoto pixels.

**INFERRED (not proven):** that Lightroom keeps B ≈ 8.5k/65535 here because
PV2012 preserves signed/wide channels until a final gamut-mapping step.
Alternative explanations consistent with the same observation: a different
baseline tone response producing larger B, a different camera transform
yielding in-gamut colors, or channel-preserving gamut handling elsewhere
in the stack. Lightroom-side evidence (E1/E2) is required before treating
this as established.

**UNKNOWN:** whether the 273,792 masked pixels' camera colors were otherwise
correct — the mask selects for B≈0 outliers by construction and cannot
establish correctness on R/G.
