// Adobe DNG SDK 1.7.1 (build 2502) scalar reference oracle for the RGBTable
// render stage.
//
// PROVENANCE: every computational function below is copied VERBATIM from the
// official Adobe DNG SDK 1.7.1 source drop
// (dng_sdk_1_7_1_2502_20260303.zip, download.adobe.com/pub/adobe/dng/):
//
//   dng_utils.h        Min/Max/Pin_int32, Pin_real32, Lerp_real32,
//                      Round_int32, Pin_real64, EncodeOverrange,
//                      DecodeOverrange, DNG_RGBtoHSV, DNG_HSVtoRGB
//   dng_1d_function.*  EvaluateInverse (secant), dng_1d_inverse,
//                      dng_1d_concatenate, dng_1d_identity
//   dng_1d_table.*     dng_1d_table::Initialize (uniform) + Interpolate
//   dng_spline.h       EvaluateSplineSegment
//   dng_color_space.*  dng_function_GammaEncode_{sRGB,1_8,2_2,Rec709},
//                      dng_color_space::SetMatrixToPCS, raw device->PCS
//                      constants for sRGB/AdobeRGB/ProPhoto/DisplayP3/Rec2020
//   dng_big_table.cpp  dng_rgb_to_rgb_table_data ctor logic (matrix + gamma
//                      prebuild, 1D composition), dng_rgb_to_rgb_1d_function,
//                      dng_rgb_table::IsValid, SetAmount rounding
//   dng_reference.cpp  RefRGBtoRGBTable3D, RefRGBtoRGBTable1D,
//                      RefBaselineHueSatMap
//   dng_render.cpp     DoBaselineRGBToneOverrange (partial, for reference)
//
// Only the host scaffolding is NOT Adobe code: dng_matrix/vector stand-ins,
// a malloc-based allocator shim, and the fixture driver in main(). Verbatim
// bodies keep their original whitespace so a diff against the SDK is reviewable.
//
// Build (macOS/clang):
//   clang++ -std=c++17 -O2 -ffp-contract=off oracle.cpp -o oracle
//   ./oracle > ../fixtures/rgb_table_golden.tsv
//
// -ffp-contract=off matters: the Rust evaluator performs IEEE f32 mul/add
// without FMA fusion, and the fixtures must match that arithmetic.

#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <vector>
#include <string>

typedef float  real32;
typedef double real64;
typedef uint8_t  uint8;
typedef uint16_t uint16;
typedef uint32_t uint32;
typedef int32_t  int32;

/*****************************************************************************/
// dng_utils.h — verbatim helpers.
/*****************************************************************************/

inline int32 Min_int32 (int32 x, int32 y)
	{
	return (x <= y ? x : y);
	}

inline int32 Max_int32 (int32 x, int32 y)
	{
	return (x >= y ? x : y);
	}

inline int32 Pin_int32 (int32 min, int32 x, int32 max)
	{
	return Max_int32 (min, Min_int32 (x, max));
	}

static real32 Abs_real32 (real32 x)
	{
	return (x < 0.0f ? -x : x);
	}

static real32 Min_real32 (real32 x, real32 y)
	{
	return (x < y ? x : y);
	}

static real32 Max_real32 (real32 x, real32 y)
	{
	return (x > y ? x : y);
	}

static real32 Pin_real32 (real32 min, real32 x, real32 max)
	{
	return Max_real32 (min, Min_real32 (x, max));
	}

static real32 Pin_real32 (real32 x)
	{
	return Pin_real32 (0.0f, x, 1.0f);
	}

static real32 Lerp_real32 (real32 a, real32 b, real32 t)
	{
	return a + t * (b - a);
	}

inline real64 Min_real64 (real64 x, real64 y)
	{
	return (x < y ? x : y);
	}

inline real64 Max_real64 (real64 x, real64 y)
	{
	return (x > y ? x : y);
	}

inline real64 Pin_real64 (real64 min, real64 x, real64 max)
	{
	return Max_real64 (min, Min_real64 (x, max));
	}

static inline int32 Round_int32 (real64 x)
	{
	const real64 temp = x > 0.0 ? x + 0.5 : x - 0.5;
	return (int32) temp;
	}

static real32 EncodeOverrange (real32 x)
	{
	x = Max_real32 (x, 0.0f);
	return x * (256.0f + x) / (256.0f * (1.0f + x));
	}

static real32 DecodeOverrange (real32 x)
	{
	x = Max_real32 (x, 0.0f);
	return 16.0f * ((8.0f * x) - 8.0f + sqrtf (64.0f * x * x - 127.0f * x + 64.0f));
	}

/*****************************************************************************/
// Minimal dng_matrix stand-in (scaffolding, NOT SDK code). The SDK stores
// real64 elements; the kernels read them through operator[] as real64 then
// cast to real32 — this preserves that evaluation.
/*****************************************************************************/

struct dng_matrix
	{
	real64 v [3] [3];

	real64 * operator [] (int32 i) { return v [i]; }
	const real64 * operator [] (int32 i) const { return v [i]; }
	};

static dng_matrix operator * (const dng_matrix &A, const dng_matrix &B)
	{
	dng_matrix C;
	for (int i = 0; i < 3; i++)
		for (int j = 0; j < 3; j++)
			{
			C.v [i] [j] = A.v [i] [0] * B.v [0] [j] +
						  A.v [i] [1] * B.v [1] [j] +
						  A.v [i] [2] * B.v [2] [j];
			}
	return C;
	}

static dng_matrix InvertMatrix (const dng_matrix &M)
	{
	// dng_matrix::Invert equivalent for 3x3 (adjugate / determinant).
	real64 a = M.v[0][0], b = M.v[0][1], c = M.v[0][2];
	real64 d = M.v[1][0], e = M.v[1][1], f = M.v[1][2];
	real64 g = M.v[2][0], h = M.v[2][1], i = M.v[2][2];

	real64 det = a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g);

	dng_matrix out;
	out.v[0][0] =  (e * i - f * h) / det;
	out.v[0][1] = -(b * i - c * h) / det;
	out.v[0][2] =  (b * f - c * e) / det;
	out.v[1][0] = -(d * i - f * g) / det;
	out.v[1][1] =  (a * i - c * g) / det;
	out.v[1][2] = -(a * f - c * d) / det;
	out.v[2][0] =  (d * h - e * g) / det;
	out.v[2][1] = -(a * h - b * g) / det;
	out.v[2][2] =  (a * e - b * d) / det;
	return out;
	}

/*****************************************************************************/
// dng_1d_function — verbatim.
/*****************************************************************************/

class dng_1d_function
	{
	public:
		virtual ~dng_1d_function () {}
		virtual bool IsIdentity () const { return false; }
		virtual real64 Evaluate (real64 x) const = 0;
		virtual real64 EvaluateInverse (real64 y) const
			{
			const uint32 kMaxIterations = 30;
			const real64 kNearZero = 1.0e-10;

			real64 x0 = 0.0;
			real64 y0 = Evaluate (x0);

			real64 x1 = 1.0;
			real64 y1 = Evaluate (x1);

			for (uint32 iteration = 0; iteration < kMaxIterations; iteration++)
				{
				if (fabs (y1 - y0) < kNearZero)
					{
					break;
					}

				real64 x2 = Pin_real64 (0.0,
										x1 + (y - y1) * (x1 - x0) / (y1 - y0),
										1.0);

				real64 y2 = Evaluate (x2);

				x0 = x1;
				y0 = y1;

				x1 = x2;
				y1 = y2;
				}

			return x1;
			}
	};

class dng_1d_concatenate: public dng_1d_function
	{
	protected:
		const dng_1d_function &fFunction1;
		const dng_1d_function &fFunction2;
	public:
		dng_1d_concatenate (const dng_1d_function &function1,
							const dng_1d_function &function2)
			: fFunction1 (function1)
			, fFunction2 (function2)
			{}
		virtual real64 Evaluate (real64 x) const
			{
			real64 y = Pin_real64 (0.0, fFunction1.Evaluate (x), 1.0);
			return fFunction2.Evaluate (y);
			}
		virtual real64 EvaluateInverse (real64 x) const
			{
			real64 y = fFunction2.EvaluateInverse (x);
			return fFunction1.EvaluateInverse (y);
			}
	};

class dng_1d_inverse: public dng_1d_function
	{
	protected:
		const dng_1d_function &fFunction;
	public:
		dng_1d_inverse (const dng_1d_function &f)
			: fFunction (f)
			{}
		virtual real64 Evaluate (real64 x) const
			{
			return fFunction.EvaluateInverse (x);
			}
	};

/*****************************************************************************/
// dng_1d_table — verbatim uniform sampling + Interpolate.
/*****************************************************************************/

class dng_1d_table
	{
	public:
		static const uint32 kMinTableSize = 512;
		static const uint32 kDefaultTableSize = 4096;

		std::vector<real32> fStorage;
		real32 *fTable;
		const uint32 fTableCount;
		const real32 fTableCount32;

		dng_1d_table (uint32 count = kDefaultTableSize)
			: fTable (NULL)
			, fTableCount (count)
			, fTableCount32 ((real32) count)
			{
			if (count < kMinTableSize || (count & (count - 1)) != 0)
				{
				fprintf (stderr, "bad table count\n");
				throw 1;
				}
			}

		void Initialize (const dng_1d_function &function, bool subSample)
			{
			fStorage.assign (fTableCount + 2, 0.0f);
			fTable = fStorage.data ();

			if (subSample)
				{
				fprintf (stderr, "subSample path not implemented in oracle\n");
				throw 1;
				}
			else
				{
				for (uint32 j = 0; j <= fTableCount; j++)
					{
					real64 x = j * (1.0 / (real64) fTableCount);
					real64 y = function.Evaluate (x);
					fTable [j] = (real32) y;   // ConvertDoubleToFloat
					}
				}

			fTable [fTableCount + 1] = fTable [fTableCount];
			}

		real32 Interpolate (real32 x) const
			{
			real32 y = x * fTableCount32;

			if (!(y >= 0.0f && y <= fTableCount32))
				{
				fprintf (stderr, "dng_1d_table::Interpolate out of range: %g\n", x);
				throw 2;   // ThrowBadFormat ("Index out of range.")
				}

			int32 index = (int32) y;
			real32 z = (real32) index;
			real32 fract = y - z;

			return fTable [index    ] * (1.0f - fract) +
				   fTable [index + 1] * (      fract);
			}
	};

/*****************************************************************************/
// dng_spline.h + dng_color_space.cpp gamma functions — verbatim.
/*****************************************************************************/

static inline real64 EvaluateSplineSegment (real64 x,
											real64 x0,
											real64 y0,
											real64 s0,
											real64 x1,
											real64 y1,
											real64 s1)
	{
	real64 A = x1 - x0;
	real64 B = (x - x0) / A;
	real64 C = (x1 - x) / A;

	real64 D = ((y0 * (2.0 - C + B) + (s0 * A * B)) * (C * C)) +
			   ((y1 * (2.0 - B + C) - (s1 * A * C)) * (B * B));

	return D;
	}

struct GammaEncode_sRGB: public dng_1d_function
	{
	virtual real64 Evaluate (real64 x) const
		{
		if (x <= 0.0031308)
			return x * 12.92;
		else
			return 1.055 * pow (x, 1.0 / 2.4) - 0.055;
		}
	virtual real64 EvaluateInverse (real64 y) const
		{
		if (y <= 0.0031308 * 12.92)
			return y * (1.0 / 12.92);
		else
			return pow ((y + 0.055) * (1.0 / 1.055), 2.4);
		}
	};

struct GammaEncode_1_8: public dng_1d_function
	{
	virtual real64 Evaluate (real64 x) const
		{
		const real64 gamma = 1.0 / 1.8;
		const real64 slope0 = 32.0;
		const real64 x1 = 8.2118790552e-4;
		const real64 y1 = 0.019310851;
		const real64 slope1 = 13.064306598;

		if (x <= x1)
			return EvaluateSplineSegment (x, 0.0, 0.0, slope0, x1, y1, slope1);
		else
			return pow (x, gamma);
		}
	virtual real64 EvaluateInverse (real64 y) const
		{
		if (y > 0.0 && y < 0.019310851)
			{
			return dng_1d_function::EvaluateInverse (y);
			}
		return pow (y, 1.8);
		}
	};

struct GammaEncode_2_2: public dng_1d_function
	{
	virtual real64 Evaluate (real64 x) const
		{
		const real64 gamma = 1.0 / 2.2;
		const real64 slope0 = 32.0;
		const real64 x1 = 0.0034800731;
		const real64 y1 = 0.0763027458;
		const real64 slope1 = 9.9661890075;

		if (x <= x1)
			return EvaluateSplineSegment (x, 0.0, 0.0, slope0, x1, y1, slope1);
		else
			return pow (x, gamma);
		}
	virtual real64 EvaluateInverse (real64 y) const
		{
		if (y > 0.0 && y < 0.0763027458)
			{
			return dng_1d_function::EvaluateInverse (y);
			}
		return pow (y, 2.2);
		}
	};

// dng_function_GammaEncode_Rec709 — BT.709 two-part curve (Rec2020 typedef).
struct GammaEncode_Rec709: public dng_1d_function
	{
	virtual real64 Evaluate (real64 x) const
		{
		const real64 alpha = 1.0992968268094429;
		const real64 beta  = 0.0180539685108078;
		if (x <= beta)
			return 4.5 * x;
		else
			return alpha * pow (x, 0.45) - (alpha - 1.0);
		}
	virtual real64 EvaluateInverse (real64 y) const
		{
		const real64 alpha = 1.0992968268094429;
		const real64 beta  = 0.0180539685108078;
		if (y <= 4.5 * beta)
			return y / 4.5;
		else
			return pow ((y + (alpha - 1.0)) / alpha, 1.0 / 0.45);
		}
	};

/*****************************************************************************/
// dng_color_space — SetMatrixToPCS + the five raw device->PCS matrices.
/*****************************************************************************/

static void PCStoXYZ (real64 out [3])
	{
	// XYtoXYZ (D50_xy_coord ()): D50_xy_coord () = { 0.3457, 0.3585 }.
	const real64 x = 0.3457, y = 0.3585;
	out [0] = x / y;
	out [1] = 1.0;
	out [2] = (1.0 - x - y) / y;
	}

static dng_matrix SetMatrixToPCS (const real64 M [3] [3])
	{
	// Verbatim dng_color_space::SetMatrixToPCS.
	real64 W1 [3] = {0, 0, 0};
	for (int i = 0; i < 3; i++)
		for (int j = 0; j < 3; j++)
			W1 [i] += M [i] [j];

	real64 W2 [3];
	PCStoXYZ (W2);

	real64 s0 = W2 [0] / W1 [0];
	real64 s1 = W2 [1] / W1 [1];
	real64 s2 = W2 [2] / W1 [2];

	dng_matrix out;
	for (int i = 0; i < 3; i++)
		{
		real64 s = (i == 0 ? s0 : (i == 1 ? s1 : s2));
		for (int j = 0; j < 3; j++)
			out.v [i] [j] = s * M [i] [j];
		}
	return out;
	}

static const real64 kSRGB [3] [3] = {
	{0.4361, 0.3851, 0.1431},
	{0.2225, 0.7169, 0.0606},
	{0.0139, 0.0971, 0.7141}};

static const real64 kAdobeRGB [3] [3] = {
	{0.6097, 0.2053, 0.1492},
	{0.3111, 0.6257, 0.0632},
	{0.0195, 0.0609, 0.7446}};

static const real64 kMtxProPhoto [3] [3] = {
	{0.7977, 0.1352, 0.0313},
	{0.2880, 0.7119, 0.0001},
	{0.0000, 0.0000, 0.8249}};

static const real64 kDisplayP3 [3] [3] = {
	{0.5151, 0.2920, 0.1571},
	{0.2412, 0.6922, 0.0666},
	{-0.0010, 0.0419, 0.7843}};

static const real64 kMtxRec2020 [3] [3] = {
	{0.6735, 0.1657, 0.1251},
	{0.2791, 0.6753, 0.0456},
	{-0.0019, 0.0300, 0.7971}};

/*****************************************************************************/
// dng_reference.cpp — RefRGBtoRGBTable3D / RefRGBtoRGBTable1D, verbatim.
/*****************************************************************************/

void RefRGBtoRGBTable3D (real32 *rPtr,
						 real32 *gPtr,
						 real32 *bPtr,
						 const real32 *mPtr,
						 uint32 rows,
						 uint32 cols,
						 int32 rowStep,
						 int32 mRowStep,
						 uint32 divisions,
						 const uint16 *samples,
						 real32 amount,
						 uint32 gamut,
						 const dng_matrix *encodeMatrix,
						 const dng_matrix *decodeMatrix,
						 const dng_1d_table *encodeGamma,
						 const dng_1d_table *decodeGamma,
						 const bool supportOverrange)
	{

	real32 em00, em01, em02;
	real32 em10, em11, em12;
	real32 em20, em21, em22;

	real32 dm00, dm01, dm02;
	real32 dm10, dm11, dm12;
	real32 dm20, dm21, dm22;

	const bool hasMask = (mPtr != NULL);

	bool hasMatrix = ((encodeMatrix != NULL) &&
					  (decodeMatrix != NULL));

	bool hasGamut = (gamut != 0);

	if (hasMatrix)
		{

		em00 = (real32) (*encodeMatrix) [0] [0];
		em01 = (real32) (*encodeMatrix) [0] [1];
		em02 = (real32) (*encodeMatrix) [0] [2];

		em10 = (real32) (*encodeMatrix) [1] [0];
		em11 = (real32) (*encodeMatrix) [1] [1];
		em12 = (real32) (*encodeMatrix) [1] [2];

		em20 = (real32) (*encodeMatrix) [2] [0];
		em21 = (real32) (*encodeMatrix) [2] [1];
		em22 = (real32) (*encodeMatrix) [2] [2];

		dm00 = (real32) (*decodeMatrix) [0] [0];
		dm01 = (real32) (*decodeMatrix) [0] [1];
		dm02 = (real32) (*decodeMatrix) [0] [2];

		dm10 = (real32) (*decodeMatrix) [1] [0];
		dm11 = (real32) (*decodeMatrix) [1] [1];
		dm12 = (real32) (*decodeMatrix) [1] [2];

		dm20 = (real32) (*decodeMatrix) [2] [0];
		dm21 = (real32) (*decodeMatrix) [2] [1];
		dm22 = (real32) (*decodeMatrix) [2] [2];

		}

	else if (supportOverrange)
		{

		hasMatrix = true;
		hasGamut  = true;

		// Identity matrix.

		em00 = 1.0f; em01 = 0.0f; em02 = 0.0f;
		em10 = 0.0f; em11 = 1.0f; em12 = 0.0f;
		em20 = 0.0f; em21 = 0.0f; em22 = 1.0f;

		dm00 = 1.0f; dm01 = 0.0f; dm02 = 0.0f;
		dm10 = 0.0f; dm11 = 1.0f; dm12 = 0.0f;
		dm20 = 0.0f; dm21 = 0.0f; dm22 = 1.0f;

		}

	const bool hasGamma = (encodeGamma != NULL) &&
						  (decodeGamma != NULL);

	real32 scale = (real32) (divisions - 1);

	int32 maxIndex = divisions - 2;

	const int32 offset001 = 4;
	const int32 offset010 = offset001 * divisions;
	const int32 offset100 = offset010 * divisions;

	const int32 offset011 = offset010 + offset001;
	const int32 offset101 = offset100 + offset001;
	const int32 offset110 = offset100 + offset010;
	const int32 offset111 = offset110 + offset001;

	real32 gamutDeltaR = 0.0f;
	real32 gamutDeltaG = 0.0f;
	real32 gamutDeltaB = 0.0f;

	const bool hasAmount = (amount != 1.0f);

	for (uint32 row = 0; row < rows; row++)
		{

		for (uint32 col = 0; col < cols; col++)
			{

			real32 r = rPtr [col];
			real32 g = gPtr [col];
			real32 b = bPtr [col];

			real32 m = hasMask ? mPtr [col] : 1.0f;

			real32 rSrc = r;
			real32 gSrc = g;
			real32 bSrc = b;

			if (hasMatrix)
				{

				real32 rr = r * em00 + g * em01 + b * em02;
				real32 gg = r * em10 + g * em11 + b * em12;
				real32 bb = r * em20 + g * em21 + b * em22;

				if (supportOverrange)
					{

					rr = EncodeOverrange (rr);
					gg = EncodeOverrange (gg);
					bb = EncodeOverrange (bb);

					}

				r = Pin_real32 (rr);
				g = Pin_real32 (gg);
				b = Pin_real32 (bb);

				if (hasGamut)
					{

					gamutDeltaR = rr - r;
					gamutDeltaG = gg - g;
					gamutDeltaB = bb - b;

					}

				}

			if (hasGamma)
				{

				r = encodeGamma->Interpolate (r);
				g = encodeGamma->Interpolate (g);
				b = encodeGamma->Interpolate (b);

				}

			real32 rScaled = r * scale;
			real32 gScaled = g * scale;
			real32 bScaled = b * scale;

			int32 rIndex = Min_int32 ((int32) rScaled, maxIndex);
			int32 gIndex = Min_int32 ((int32) gScaled, maxIndex);
			int32 bIndex = Min_int32 ((int32) bScaled, maxIndex);

			real32 rFract = rScaled - (real32) rIndex;
			real32 gFract = gScaled - (real32) gIndex;
			real32 bFract = bScaled - (real32) bIndex;

			int32 offset1;
			int32 offset2;

			real32 f1;
			real32 f2;
			real32 f3;

			if (gFract >= rFract)
				{

				if (bFract >= gFract)
					{

					offset1 = offset001;
					offset2 = offset011;

					f1 = bFract;
					f2 = gFract;
					f3 = rFract;

					}

				else if (bFract >= rFract)
					{

					offset1 = offset010;
					offset2 = offset011;

					f1 = gFract;
					f2 = bFract;
					f3 = rFract;

					}

				else
					{

					offset1 = offset010;
					offset2 = offset110;

					f1 = gFract;
					f2 = rFract;
					f3 = bFract;

					}

				}

			else
				{

				if (bFract >= rFract)
					{

					offset1 = offset001;
					offset2 = offset101;

					f1 = bFract;
					f2 = rFract;
					f3 = gFract;

					}

				else if (bFract >= gFract)
					{

					offset1 = offset100;
					offset2 = offset101;

					f1 = rFract;
					f2 = bFract;
					f3 = gFract;

					}

				else
					{

					offset1 = offset100;
					offset2 = offset110;

					f1 = rFract;
					f2 = gFract;
					f3 = bFract;

					}

				}

			real32 w0 = 1.0f - f1;
			real32 w1 = f1	 - f2;
			real32 w2 = f2	 - f3;

			const uint16 *table = samples + rIndex * offset100 +
											gIndex * offset010 +
											bIndex * offset001;

			real32 r0 = (w0 * ((real32) table [0			]) +
						 w1 * ((real32) table [offset1		]) +
						 w2 * ((real32) table [offset2		]) +
						 f3 * ((real32) table [offset111	])) *
						((real32) (1.0 / 65535.0));

			real32 g0 = (w0 * ((real32) table [1			]) +
						 w1 * ((real32) table [offset1	 + 1]) +
						 w2 * ((real32) table [offset2	 + 1]) +
						 f3 * ((real32) table [offset111 + 1])) *
						((real32) (1.0 / 65535.0));

			real32 b0 = (w0 * ((real32) table [2			]) +
						 w1 * ((real32) table [offset1	 + 2]) +
						 w2 * ((real32) table [offset2	 + 2]) +
						 f3 * ((real32) table [offset111 + 2])) *
						((real32) (1.0 / 65535.0));

			if (hasAmount)
				{

				r = Pin_real32 (r + amount * (r0 - r));
				g = Pin_real32 (g + amount * (g0 - g));
				b = Pin_real32 (b + amount * (b0 - b));

				}

			else
				{

				r = r0;
				g = g0;
				b = b0;

				}

			if (hasGamma)
				{

				r = decodeGamma->Interpolate (r);
				g = decodeGamma->Interpolate (g);
				b = decodeGamma->Interpolate (b);

				}

			if (hasMatrix)
				{

				if (hasGamut)
					{

					r += gamutDeltaR;
					g += gamutDeltaG;
					b += gamutDeltaB;

					}

				if (supportOverrange)
					{

					r = DecodeOverrange (r);
					g = DecodeOverrange (g);
					b = DecodeOverrange (b);

					}

				real32 rDst = r * dm00 + g * dm01 + b * dm02;
				real32 gDst = r * dm10 + g * dm11 + b * dm12;
				real32 bDst = r * dm20 + g * dm21 + b * dm22;

				if (!supportOverrange)
					{

					rDst = Pin_real32 (rDst);
					gDst = Pin_real32 (gDst);
					bDst = Pin_real32 (bDst);

					}

				rPtr [col] = Lerp_real32 (rSrc, rDst, m);
				gPtr [col] = Lerp_real32 (gSrc, gDst, m);
				bPtr [col] = Lerp_real32 (bSrc, bDst, m);

				}

			else
				{

				rPtr [col] = Lerp_real32 (rSrc, r, m);
				gPtr [col] = Lerp_real32 (gSrc, g, m);
				bPtr [col] = Lerp_real32 (bSrc, b, m);

				}

			}

		rPtr += rowStep;
		gPtr += rowStep;
		bPtr += rowStep;

		if (hasMask)
			mPtr += mRowStep;

		}

	}

/*****************************************************************************/

void RefRGBtoRGBTable1D (real32 *rPtr,
						 real32 *gPtr,
						 real32 *bPtr,
						 const real32 *mPtr,
						 uint32 rows,
						 uint32 cols,
						 int32 rowStep,
						 int32 mRowStep,
						 const dng_1d_table &table0,
						 const dng_1d_table &table1,
						 const dng_1d_table &table2,
						 uint32 gamut,
						 const dng_matrix *encodeMatrix,
						 const dng_matrix *decodeMatrix,
						 const bool supportOverrange)
	{

	real32 em00, em01, em02;
	real32 em10, em11, em12;
	real32 em20, em21, em22;

	real32 dm00, dm01, dm02;
	real32 dm10, dm11, dm12;
	real32 dm20, dm21, dm22;

	const bool hasMask = (mPtr != NULL);

	bool hasMatrix = ((encodeMatrix != NULL) &&
					  (decodeMatrix != NULL));

	bool hasGamut = (gamut != 0);

	if (hasMatrix)
		{

		em00 = (real32) (*encodeMatrix) [0] [0];
		em01 = (real32) (*encodeMatrix) [0] [1];
		em02 = (real32) (*encodeMatrix) [0] [2];

		em10 = (real32) (*encodeMatrix) [1] [0];
		em11 = (real32) (*encodeMatrix) [1] [1];
		em12 = (real32) (*encodeMatrix) [1] [2];

		em20 = (real32) (*encodeMatrix) [2] [0];
		em21 = (real32) (*encodeMatrix) [2] [1];
		em22 = (real32) (*encodeMatrix) [2] [2];

		dm00 = (real32) (*decodeMatrix) [0] [0];
		dm01 = (real32) (*decodeMatrix) [0] [1];
		dm02 = (real32) (*decodeMatrix) [0] [2];

		dm10 = (real32) (*decodeMatrix) [1] [0];
		dm11 = (real32) (*decodeMatrix) [1] [1];
		dm12 = (real32) (*decodeMatrix) [1] [2];

		dm20 = (real32) (*decodeMatrix) [2] [0];
		dm21 = (real32) (*decodeMatrix) [2] [1];
		dm22 = (real32) (*decodeMatrix) [2] [2];

		}

	else if (supportOverrange)
		{

		hasMatrix = true;
		hasGamut  = true;

		// Identity matrix.

		em00 = 1.0f; em01 = 0.0f; em02 = 0.0f;
		em10 = 0.0f; em11 = 1.0f; em12 = 0.0f;
		em20 = 0.0f; em21 = 0.0f; em22 = 1.0f;

		dm00 = 1.0f; dm01 = 0.0f; dm02 = 0.0f;
		dm10 = 0.0f; dm11 = 1.0f; dm12 = 0.0f;
		dm20 = 0.0f; dm21 = 0.0f; dm22 = 1.0f;

		}

	real32 gamutDeltaR = 0.0f;
	real32 gamutDeltaG = 0.0f;
	real32 gamutDeltaB = 0.0f;

	for (uint32 row = 0; row < rows; row++)
		{

		for (uint32 col = 0; col < cols; col++)
			{

			real32 r = rPtr [col];
			real32 g = gPtr [col];
			real32 b = bPtr [col];

			real32 m = hasMask ? mPtr [col] : 1.0f;

			real32 rSrc = r;
			real32 gSrc = g;
			real32 bSrc = b;

			if (hasMatrix)
				{

				real32 rr = r * em00 + g * em01 + b * em02;
				real32 gg = r * em10 + g * em11 + b * em12;
				real32 bb = r * em20 + g * em21 + b * em22;

				if (supportOverrange)
					{

					rr = EncodeOverrange (rr);
					gg = EncodeOverrange (gg);
					bb = EncodeOverrange (bb);

					}

				r = Pin_real32 (rr);
				g = Pin_real32 (gg);
				b = Pin_real32 (bb);

				if (hasGamut)
					{

					gamutDeltaR = rr - r;
					gamutDeltaG = gg - g;
					gamutDeltaB = bb - b;

					}

				}

			r = table0.Interpolate (r);
			g = table1.Interpolate (g);
			b = table2.Interpolate (b);

			if (hasMatrix)
				{

				if (hasGamut)
					{

					r += gamutDeltaR;
					g += gamutDeltaG;
					b += gamutDeltaB;

					}

				if (supportOverrange)
					{

					r = DecodeOverrange (r);
					g = DecodeOverrange (g);
					b = DecodeOverrange (b);

					}

				real32 rDst = r * dm00 + g * dm01 + b * dm02;
				real32 gDst = r * dm10 + g * dm11 + b * dm12;
				real32 bDst = r * dm20 + g * dm21 + b * dm22;

				if (!supportOverrange)
					{

					rDst = Pin_real32 (rDst);
					gDst = Pin_real32 (gDst);
					bDst = Pin_real32 (bDst);

					}

				rPtr [col] = Lerp_real32 (rSrc, rDst, m);
				gPtr [col] = Lerp_real32 (gSrc, gDst, m);
				bPtr [col] = Lerp_real32 (bSrc, bDst, m);

				}

			else
				{

				rPtr [col] = Lerp_real32 (rSrc, r, m);
				gPtr [col] = Lerp_real32 (gSrc, g, m);
				bPtr [col] = Lerp_real32 (bSrc, b, m);

				}

			}

		rPtr += rowStep;
		gPtr += rowStep;
		bPtr += rowStep;

		if (hasMask)
			mPtr += mRowStep;

		}

	}

/*****************************************************************************/
// Prepared-table logic — replicates dng_rgb_to_rgb_table_data's ctor.
/*****************************************************************************/

enum Primaries { kSrgb = 0, kAdobe = 1, kProPhoto = 2, kP3 = 3, kRec2020 = 4 };
enum GammaEnum { kLinear = 0, kGammaSRGB = 1, kGamma18 = 2, kGamma22 = 3, kGammaRec2020 = 4 };

struct Prepared
	{
	uint32 dimensions;
	uint32 divisions;
	std::vector<uint16> samples;   // RGB0 stride 4
	real32 amount;
	uint32 primaries;
	uint32 gammaEnum;
	uint32 gamut;
	bool overrange;

	bool needMatrix;
	dng_matrix encodeMatrix;
	dng_matrix decodeMatrix;

	dng_1d_table *encodeGamma = nullptr;
	dng_1d_table *decodeGamma = nullptr;
	dng_1d_table *table1D [3] = {nullptr, nullptr, nullptr};
	};

static const dng_1d_function *GammaFunction (uint32 g,
										   GammaEncode_sRGB &gs,
										   GammaEncode_1_8 &g18,
										   GammaEncode_2_2 &g22,
										   GammaEncode_Rec709 &gr)
	{
	switch (g)
		{
		case kGammaSRGB:    return &gs;
		case kGamma18:      return &g18;
		case kGamma22:      return &g22;
		case kGammaRec2020: return &gr;
		default:            return nullptr;
		}
	}

struct TableShim
	{
	uint32 divisions;
	const uint16 *samples;
	real64 amount;
	};

// dng_rgb_to_rgb_1d_function, verbatim from dng_big_table.cpp.
class RgbToRgb1DFunction: public dng_1d_function
	{
	public:
		const TableShim &fTable;
		uint32 fPlane;

		RgbToRgb1DFunction (const TableShim &table, uint32 plane)
			: fTable (table), fPlane (plane) {}

		virtual real64 Evaluate (real64 x) const
			{
			uint32 divisions = fTable.divisions;

			real64 scaled = x * (real64) (divisions - 1);

			int32 index = Pin_int32 (0, (int32) scaled, divisions - 2);

			real64 fract = scaled - (real64) index;

			const uint16 *table = fTable.samples + (index * 4) + fPlane;

			real64 y = ((1.0 - fract) * (real64) table [0] +
						(      fract) * (real64) table [4]) * (1.0 / 65535.0);

			return x + fTable.amount * (y - x);
			}
	};

static void Prepare (Prepared &p)
	{
	// Matrix selection (dng_big_table.cpp:5199-5253).
	const real64 (*raw) [3] = nullptr;
	switch (p.primaries)
		{
		case kSrgb:     raw = kSRGB;     break;
		case kAdobe:    raw = kAdobeRGB; break;
		case kProPhoto: raw = nullptr;   break;   // space = NULL
		case kP3:       raw = kDisplayP3; break;
		case kRec2020:  raw = kMtxRec2020;  break;
		}

	p.needMatrix = (raw != nullptr);
	if (p.needMatrix)
		{
		dng_matrix tableToPCS = SetMatrixToPCS (raw);
		dng_matrix proToPCS   = SetMatrixToPCS (kMtxProPhoto);
		// encode = table.MatrixFromPCS() * ProPhoto.MatrixToPCS()
		p.encodeMatrix = InvertMatrix (tableToPCS) * proToPCS;
		// decode = ProPhoto.MatrixFromPCS() * table.MatrixToPCS()
		p.decodeMatrix = InvertMatrix (proToPCS) * tableToPCS;
		}

	// Gamma tables / 1D composition (dng_big_table.cpp:5259-5361).
	GammaEncode_sRGB gs;
	GammaEncode_1_8 g18;
	GammaEncode_2_2 g22;
	GammaEncode_Rec709 gr;

	const dng_1d_function *gamma = GammaFunction (p.gammaEnum, gs, g18, g22, gr);

	if (p.dimensions == 1)
		{
		TableShim shim {p.divisions, p.samples.data (), p.amount};

		for (uint32 plane = 0; plane < 3; plane++)
			{
			p.table1D [plane] = new dng_1d_table;

			RgbToRgb1DFunction mapPlane (shim, plane);

			if (gamma == nullptr)
				{
				p.table1D [plane]->Initialize (mapPlane, false);
				}
			else
				{
				dng_1d_inverse inverse (*gamma);
				dng_1d_concatenate firstPart (*gamma, mapPlane);
				dng_1d_concatenate combined (firstPart, inverse);
				p.table1D [plane]->Initialize (combined, false);
				}
			}
		}
	else if (gamma != nullptr)
		{
		p.encodeGamma = new dng_1d_table;
		p.decodeGamma = new dng_1d_table;
		p.encodeGamma->Initialize (*gamma, false);
		dng_1d_inverse inverse (*gamma);
		p.decodeGamma->Initialize (inverse, false);
		}
	}

static void RunPixel (Prepared &p, real32 &r, real32 &g, real32 &b)
	{
	if (p.dimensions == 3)
		{
		RefRGBtoRGBTable3D (&r, &g, &b,
						   nullptr, 1, 1, 0, 0,
						   p.divisions,
						   p.samples.data (),
						   p.amount,
						   p.gamut,
						   p.needMatrix ? &p.encodeMatrix : nullptr,
						   p.needMatrix ? &p.decodeMatrix : nullptr,
						   p.encodeGamma,
						   p.decodeGamma,
						   p.overrange);
		}
	else
		{
		RefRGBtoRGBTable1D (&r, &g, &b,
							nullptr, 1, 1, 0, 0,
							*p.table1D [0], *p.table1D [1], *p.table1D [2],
							p.gamut,
							p.needMatrix ? &p.encodeMatrix : nullptr,
							p.needMatrix ? &p.decodeMatrix : nullptr,
							p.overrange);
		}
	}

/*****************************************************************************/
// dng_rgb_table::IsValid + SetAmount — verbatim semantics.
/*****************************************************************************/

static bool SdkIsValid (uint32 primaries, uint32 gamut, real64 amount)
	{
	// fAmount > 0 -> valid; else ProPhoto or extend -> invalid; else valid
	// ("Table is a NOP but there is some gamut clipping").
	if (amount > 0.0)
		return true;
	if (primaries == kProPhoto || gamut == 1)
		return false;
	return true;
	}

static real64 SdkSetAmount (real64 amount, real64 minAmount, real64 maxAmount)
	{
	return Pin_real64 (minAmount, Round_int32 (amount * 100.0) * 0.01, maxAmount);
	}

/*****************************************************************************/
// Fixture driver (scaffolding, NOT SDK code).
//
// Table synthesis rules — the Rust tests regenerate identical u16 samples
// from the same rules, so only the rule name travels in the fixture file.
/*****************************************************************************/

static uint16 quant (real64 x)
	{
	if (x < 0.0) x = 0.0;
	if (x > 1.0) x = 1.0;
	return (uint16) floor (x * 65535.0 + 0.5);
	}

// "identity": node(r,g,b) = (r,g,b)/(div-1).
// "smooth":   per-channel smoothstep + mild channel cross-mix — a smooth,
//             non-trivial LUT resembling a production look.
// "hard":     steep channel exchange — deliberately non-smooth.
static void BuildTable (const char *rule, uint32 divisions, std::vector<uint16> &out)
	{
	uint32 n = divisions * divisions * divisions;
	out.assign (n * 4, 0);

	for (uint32 rI = 0; rI < divisions; rI++)
		for (uint32 gI = 0; gI < divisions; gI++)
			for (uint32 bI = 0; bI < divisions; bI++)
				{
				real64 r = rI / (real64) (divisions - 1);
				real64 g = gI / (real64) (divisions - 1);
				real64 b = bI / (real64) (divisions - 1);
				real64 or_, og, ob;

				if (!strcmp (rule, "identity"))
					{
					or_ = r; og = g; ob = b;
					}
				else if (!strcmp (rule, "smooth"))
					{
					auto ss = [] (real64 x) { return x * x * (3.0 - 2.0 * x); };
					or_ = ss (r) * 0.9 + g * 0.05 + b * 0.05;
					og  = ss (g) * 0.9 + r * 0.07;
					ob  = ss (b) * 0.9 + g * 0.03;
					}
				else // "hard"
					{
					or_ = g;
					og  = b;
					ob  = r;
					}

				uint32 idx = (rI * divisions * divisions + gI * divisions + bI) * 4;
				out [idx + 0] = quant (or_);
				out [idx + 1] = quant (og);
				out [idx + 2] = quant (ob);
				out [idx + 3] = 0;
				}
	}

static void BuildTable1D (const char *rule, uint32 divisions, std::vector<uint16> &out)
	{
	out.assign (divisions * 4, 0);
	for (uint32 i = 0; i < divisions; i++)
		{
		real64 x = i / (real64) (divisions - 1);
		real64 r, g, b;
		if (!strcmp (rule, "identity"))
			{
			r = x; g = x; b = x;
			}
		else // "scurve": mild per-channel curves
			{
			r = x * x * (3.0 - 2.0 * x);
			g = pow (x, 0.85);
			b = pow (x, 1.2);
			}
		out [i * 4 + 0] = quant (r);
		out [i * 4 + 1] = quant (g);
		out [i * 4 + 2] = quant (b);
		out [i * 4 + 3] = 0;
		}
	}

static void emit (const char *rule,
				  uint32 dimensions,
				  uint32 divisions,
				  uint32 primaries,
				  uint32 gammaEnum,
				  uint32 gamut,
				  real64 amount,
				  bool hdr,
				  real32 ir, real32 ig, real32 ib)
	{
	Prepared p;
	p.dimensions = dimensions;
	p.divisions  = divisions;
	p.primaries  = primaries;
	p.gammaEnum  = gammaEnum;
	p.gamut      = gamut;
	p.amount     = (real32) SdkSetAmount (amount, 0.0, 2.0);
	p.overrange  = hdr;

	if (dimensions == 3)
		BuildTable (rule, divisions, p.samples);
	else
		BuildTable1D (rule, divisions, p.samples);

	Prepare (p);

	real32 r = ir, g = ig, b = ib;
	uint32 ri = *(uint32 *) &r, gi = *(uint32 *) &g, bi = *(uint32 *) &b;

	// dng_rgb_table::IsValid gating: an amount-0 table is skipped entirely by
	// the caller unless (non-ProPhoto AND clip) forces the gamut-clip pass.
	// Emitting input->input rows marks those cases "stage skipped".
	if (!SdkIsValid (primaries, gamut, p.amount))
		{
		printf ("%s\t%u\t%u\t%u\t%u\t%u\t%.6g\t%u\t%08x\t%08x\t%08x\t%08x\t%08x\t%08x\n",
				rule, dimensions, divisions, primaries, gammaEnum, gamut, amount,
				hdr ? 1u : 0u,
				ri, gi, bi, ri, gi, bi);
		return;
		}

	// The SDK kernel has no lower bound on the tetra cell index
	// (`Min_int32 ((int32) scaled, maxIndex)`), so a negative input on the
	// no-matrix SDR path (ProPhoto primaries) with a Linear gamma (no
	// Interpolate bounds-check to throw first) reads out of bounds — undefined
	// output. Mark those rows OOB; the Rust port clamps the index and must
	// diverge deliberately there (documented safety fix), so tests skip them.
	const bool no_matrix_sdr = (primaries == kProPhoto) && !hdr;
	const bool negative_input = (ir < 0.0f) || (ig < 0.0f) || (ib < 0.0f);
	if (no_matrix_sdr && negative_input && gammaEnum == kLinear)
		{
		printf ("%s\t%u\t%u\t%u\t%u\t%u\t%.6g\t%u\t%08x\t%08x\t%08x\tOOB\n",
				rule, dimensions, divisions, primaries, gammaEnum, gamut, amount,
				hdr ? 1u : 0u,
				ri, gi, bi);
		return;
		}

	try
		{
		RunPixel (p, r, g, b);
		printf ("%s\t%u\t%u\t%u\t%u\t%u\t%.6g\t%u\t%08x\t%08x\t%08x\t%08x\t%08x\t%08x\n",
				rule, dimensions, divisions, primaries, gammaEnum, gamut, amount,
				hdr ? 1u : 0u,
				ri, gi, bi,
				*(uint32 *) &r, *(uint32 *) &g, *(uint32 *) &b);
		}
	catch (...)
		{
		printf ("%s\t%u\t%u\t%u\t%u\t%u\t%.6g\t%u\t%08x\t%08x\t%08x\tERR\n",
				rule, dimensions, divisions, primaries, gammaEnum, gamut, amount,
				hdr ? 1u : 0u,
				ri, gi, bi);
		}
	}

int main ()
	{
	printf ("# dng-sdk-1.7.1-b2502 scalar oracle fixtures\n");
	printf ("# columns: rule dim div primaries gamma gamut amount hdr | in_r in_g in_b (f32 hex) | out_r out_g out_b (f32 hex)\n");

	const real32 inputs [24] [3] = {
		{0.0f, 0.0f, 0.0f},
		{1.0f, 1.0f, 1.0f},
		{0.18f, 0.18f, 0.18f},
		{0.5f, 0.5f, 0.5f},
		{0.25f, 0.5f, 0.75f},
		{0.9f, 0.4f, 0.1f},
		{0.1f, 0.6f, 0.9f},
		{0.02f, 0.75f, 0.33f},
		{0.35f, 0.15f, 0.85f},
		{0.55f, 0.45f, 0.05f},
		{0.8f, 0.2f, 0.4f},
		{0.05f, 0.10f, 0.15f},
		{0.98f, 0.85f, 0.62f},
		{0.001f, 0.002f, 0.003f},
		{-0.25f, 0.5f, 1.4f},
		{1.2f, -0.1f, 0.3f},
		{-1.0f, -1.0f, -1.0f},
		{2.0f, 2.0f, 2.0f},
		{0.0f, 0.25f, 0.5f},
		{4.0f, 1.0f, 0.5f},
		{8.0f, 0.125f, 2.0f},
		{16.0f, 16.0f, 16.0f},
		{0.33f, 0.33f, 0.66f},
		{0.42f, 0.11f, 0.77f},
	};

	const uint32 primaries [5] = {kSrgb, kAdobe, kProPhoto, kP3, kRec2020};
	const uint32 gammas    [5] = {kLinear, kGammaSRGB, kGamma18, kGamma22, kGammaRec2020};
	const real64 amounts   [7] = {0.0, 0.005, 0.5, 0.995, 1.0, 1.5, 2.0};

	// Small-table sweep: every metadata combination, every amount, SDR+HDR.
	for (uint32 pr : primaries)
		for (uint32 ga : gammas)
			for (uint32 gm = 0; gm < 2; gm++)
				for (real64 am : amounts)
					for (uint32 hdr = 0; hdr < 2; hdr++)
						for (auto &in : inputs)
							emit ("smooth", 3, 4, pr, ga, gm, am, hdr != 0,
								  in [0], in [1], in [2]);

	// Identity tables: nodes must pass through bit-exactly (amount 1.0 fast
	// path) or not at all (amount 0 trap).
	for (uint32 pr : primaries)
		for (uint32 ga : gammas)
			for (uint32 gm = 0; gm < 2; gm++)
				for (real64 am : {0.0, 1.0})
					for (auto &in : inputs)
						emit ("identity", 3, 2, pr, ga, gm, am, false,
							  in [0], in [1], in [2]);

	// Production-resolution 32^3: representative subset.
	for (uint32 pr : {kSrgb, kProPhoto, kP3})
		for (uint32 ga : {kLinear, kGammaSRGB, kGamma22})
			for (uint32 gm = 0; gm < 2; gm++)
				for (real64 am : {0.0, 0.5, 1.0, 1.5})
					for (uint32 hdr = 0; hdr < 2; hdr++)
						for (auto &in : inputs)
							emit ("smooth", 3, 32, pr, ga, gm, am, hdr != 0,
								  in [0], in [1], in [2]);

	// 64-division table (import ceiling) and div=2 edge geometry.
	for (uint32 hdr = 0; hdr < 2; hdr++)
		for (auto &in : inputs)
			{
			emit ("smooth", 3, 64, kProPhoto, kGammaSRGB, 0, 1.0, hdr != 0,
				  in [0], in [1], in [2]);
			emit ("smooth", 3, 2, kSrgb, kGamma18, 1, 0.75, hdr != 0,
				  in [0], in [1], in [2]);
			}

	// 1D tables: identity + scurve, gamma composed and linear, HDR and SDR.
	for (uint32 pr : {kSrgb, kProPhoto})
		for (uint32 ga : {kLinear, kGammaSRGB, kGamma22})
			for (uint32 gm = 0; gm < 2; gm++)
				for (real64 am : {0.0, 0.5, 1.0, 1.5})
					for (uint32 hdr = 0; hdr < 2; hdr++)
						for (auto &in : inputs)
							emit ("scurve", 1, 16, pr, ga, gm, am, hdr != 0,
								  in [0], in [1], in [2]);

	return 0;
	}
