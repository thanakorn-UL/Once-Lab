import { DEFAULT_XMP_PROFILE_AMOUNT_PERCENT } from '../store/useEditorStore';

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

/** The flat editor fields the profile part of a develop state is projected onto. */
export interface DevelopProfileFields {
  path: string | null;
  name: string | null;
  uuid: string | null;
  amountPercent: number;
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
