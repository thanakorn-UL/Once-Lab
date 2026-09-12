/** Default Profile Amount, which renders a profile exactly as authored. */
export const DEFAULT_XMP_PROFILE_AMOUNT_PERCENT = 100;

/** Schema version of the persisted develop state; mirrors DEVELOP_STATE_VERSION in Rust. */
export const DEVELOP_STATE_VERSION = 1;

/** Wire shape of a saved profile reference; mirrors Rust `DevelopProfileReference`. */
export interface DevelopProfileReference {
  uuid: string | null;
  path: string | null;
  name: string | null;
  amount_percent: number;
}

/** Wire shape of the persisted per-image develop state; mirrors Rust `DevelopState`. */
export interface DevelopState {
  version: number;
  profile: DevelopProfileReference | null;
}

/** Runtime status of the open image's profile. */
export type XmpProfileStatus = 'none' | 'resolved' | 'missing';

/** The editor's whole view of the open image's profile: a projection of its develop state. */
export interface DevelopProfileProjection {
  status: XmpProfileStatus;
  path: string | null;
  name: string | null;
  uuid: string | null;
  amountPercent: number;
  supportsAmount: boolean;
}

/** Wire shape of `resolve_image_develop_profile`; mirrors Rust `DevelopProfileRestore`. */
export type DevelopProfileRestore =
  | { status: 'none' }
  | { status: 'resolved'; reference: DevelopProfileReference; summary: XmpProfileSummary }
  | { status: 'missing'; reference: DevelopProfileReference };

export interface XmpProfileSummary {
  name: string;
  path: string;
  uuid: string;
  supports_amount: boolean;
  convert_to_grayscale: boolean;
}

/** The flat editor fields the profile part of a develop state is projected onto. */
export interface DevelopProfileFields {
  path: string | null;
  name: string | null;
  uuid: string | null;
  amountPercent: number;
}

/** The projection of an image that has no profile. */
export const PROFILE_PROJECTION_NONE: DevelopProfileProjection = {
  status: 'none',
  path: null,
  name: null,
  uuid: null,
  amountPercent: DEFAULT_XMP_PROFILE_AMOUNT_PERCENT,
  supportsAmount: false,
};

/** Projects a restore outcome onto the editor's profile view. */
export function profileProjectionFromRestore(
  restore: DevelopProfileRestore | null | undefined,
): DevelopProfileProjection {
  if (!restore) return PROFILE_PROJECTION_NONE;

  if (restore.status === 'resolved') {
    const { reference, summary } = restore;

    return {
      status: 'resolved',
      path: summary.path,
      name: summary.name,
      uuid: reference.uuid ?? summary.uuid,
      amountPercent: Number.isFinite(reference.amount_percent)
        ? reference.amount_percent
        : DEFAULT_XMP_PROFILE_AMOUNT_PERCENT,
      supportsAmount: summary.supports_amount,
    };
  }

  if (restore.status === 'missing') {
    const { reference } = restore;

    return {
      status: 'missing',
      path: reference.path ?? null,
      name: reference.name ?? null,
      uuid: reference.uuid ?? null,
      amountPercent: Number.isFinite(reference.amount_percent)
        ? reference.amount_percent
        : DEFAULT_XMP_PROFILE_AMOUNT_PERCENT,
      supportsAmount: false,
    };
  }

  return PROFILE_PROJECTION_NONE;
}

/** Projects a freshly requested transaction target (a profile the user just picked or a clear). */
export function profileProjectionFromSelection(
  path: string | null,
  name: string | null,
  uuid: string | null,
  amountPercent: number,
  supportsAmount: boolean,
): DevelopProfileProjection {
  if (!path) return PROFILE_PROJECTION_NONE;

  return {
    status: 'resolved',
    path,
    name: name ?? null,
    uuid: uuid ?? null,
    amountPercent: Number.isFinite(amountPercent) ? amountPercent : DEFAULT_XMP_PROFILE_AMOUNT_PERCENT,
    supportsAmount,
  };
}

/** The path the backend should develop with, or null when nothing may be applied. */
export function activeProfilePath(projection: DevelopProfileProjection | null | undefined): string | null {
  if (!projection || projection.status !== 'resolved') return null;

  return projection.path;
}

/** Field-wise equality, for deciding whether the cached view still matches disk. */
export function profileProjectionsEqual(
  a: DevelopProfileProjection | null | undefined,
  b: DevelopProfileProjection | null | undefined,
): boolean {
  const left = a ?? PROFILE_PROJECTION_NONE;
  const right = b ?? PROFILE_PROJECTION_NONE;

  return (
    left.status === right.status &&
    left.path === right.path &&
    left.name === right.name &&
    left.uuid === right.uuid &&
    left.amountPercent === right.amountPercent &&
    left.supportsAmount === right.supportsAmount
  );
}

/** Projects the editor's flat profile fields onto the persisted record. */
export function developStateFromProfileFields(fields: DevelopProfileFields): DevelopState {
  const uuid = fields.uuid || null;
  const path = fields.path || null;
  const name = fields.name || null;

  if (!uuid && !path) {
    return { version: DEVELOP_STATE_VERSION, profile: null };
  }

  return {
    version: DEVELOP_STATE_VERSION,
    profile: { uuid, path, name, amount_percent: fields.amountPercent },
  };
}

/** Reads a `DevelopProfileReference` out of a restore payload, or null when unusable. */
function readRestoreReference(value: unknown): DevelopProfileReference | null {
  if (typeof value !== 'object' || value === null) return null;

  const raw = value as { uuid?: unknown; path?: unknown; name?: unknown; amount_percent?: unknown };

  return {
    uuid: typeof raw.uuid === 'string' ? raw.uuid : null,
    path: typeof raw.path === 'string' ? raw.path : null,
    name: typeof raw.name === 'string' ? raw.name : null,
    amount_percent:
      typeof raw.amount_percent === 'number' && Number.isFinite(raw.amount_percent)
        ? raw.amount_percent
        : DEFAULT_XMP_PROFILE_AMOUNT_PERCENT,
  };
}

/** Reads the profile `summary` out of a resolved restore, or null when unusable. */
function readRestoreSummary(value: unknown): XmpProfileSummary | null {
  if (typeof value !== 'object' || value === null) return null;

  const raw = value as {
    name?: unknown;
    path?: unknown;
    uuid?: unknown;
    supports_amount?: unknown;
    convert_to_grayscale?: unknown;
  };

  // A resolved profile always carries the path the backend resolved it to; without
  // one the projection has nothing to apply, so treat it as malformed.
  if (typeof raw.path !== 'string') return null;

  return {
    name: typeof raw.name === 'string' ? raw.name : '',
    path: raw.path,
    uuid: typeof raw.uuid === 'string' ? raw.uuid : '',
    supports_amount: raw.supports_amount === true,
    convert_to_grayscale: raw.convert_to_grayscale === true,
  };
}

/** Reads an `XmpDevelopReadOutcome` payload from `read_develop_state_from_xmp`. */
export function readXmpOutcomeRestore(outcome: unknown): DevelopProfileRestore | null {
  if (typeof outcome !== 'object' || outcome === null) return null;

  const restore = (outcome as { restore?: unknown }).restore;
  if (typeof restore !== 'object' || restore === null) return null;

  const record = restore as { status?: unknown; reference?: unknown; summary?: unknown };

  if (record.status === 'none') {
    return { status: 'none' };
  }

  if (record.status === 'missing') {
    const reference = readRestoreReference(record.reference);
    return reference ? { status: 'missing', reference } : null;
  }

  if (record.status === 'resolved') {
    const reference = readRestoreReference(record.reference);
    const summary = readRestoreSummary(record.summary);
    return reference && summary ? { status: 'resolved', reference, summary } : null;
  }

  // Never throws: any other payload is malformed, so the caller fails loudly
  // rather than applying a half-built projection.
  return null;
}
