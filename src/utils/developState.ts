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

/** Projects a persisted record back onto the flat editor fields. */
export function profileFieldsFromDevelopState(state: DevelopState | null | undefined): DevelopProfileFields {
  if (!state || !state.profile) {
    return {
      path: null,
      name: null,
      uuid: null,
      amountPercent: DEFAULT_XMP_PROFILE_AMOUNT_PERCENT,
    };
  }

  const { uuid, path, name, amount_percent } = state.profile;

  return {
    path: path ?? null,
    name: name ?? null,
    uuid: uuid ?? null,
    amountPercent: Number.isFinite(amount_percent) ? amount_percent : DEFAULT_XMP_PROFILE_AMOUNT_PERCENT,
  };
}

/** Reads the develop state out of a `load_metadata` payload, tolerating legacy documents. */
export function readDevelopState(metadata: unknown): DevelopState {
  if (typeof metadata !== 'object' || metadata === null) {
    return { version: DEVELOP_STATE_VERSION, profile: null };
  }

  const record = (metadata as { develop_state?: unknown }).develop_state;
  if (typeof record !== 'object' || record === null) {
    return { version: DEVELOP_STATE_VERSION, profile: null };
  }

  const raw = record as { version?: unknown; profile?: unknown };

  const version = typeof raw.version === 'number' && Number.isFinite(raw.version) ? raw.version : DEVELOP_STATE_VERSION;

  let profile: DevelopProfileReference | null = null;
  if (typeof raw.profile === 'object' && raw.profile !== null) {
    const rawProfile = raw.profile as {
      uuid?: unknown;
      path?: unknown;
      name?: unknown;
      amount_percent?: unknown;
    };

    profile = {
      uuid: typeof rawProfile.uuid === 'string' ? rawProfile.uuid : null,
      path: typeof rawProfile.path === 'string' ? rawProfile.path : null,
      name: typeof rawProfile.name === 'string' ? rawProfile.name : null,
      amount_percent:
        typeof rawProfile.amount_percent === 'number' && Number.isFinite(rawProfile.amount_percent)
          ? rawProfile.amount_percent
          : DEFAULT_XMP_PROFILE_AMOUNT_PERCENT,
    };
  }

  return { version, profile };
}
