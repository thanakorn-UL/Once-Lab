import { create } from 'zustand';
import { Adjustments, INITIAL_ADJUSTMENTS, MaskContainer, AiPatch } from '../utils/adjustments';
import { SelectedImage, WaveformData, BrushSettings } from '../components/ui/AppProperties';
import { ChannelConfig } from '../components/adjustments/Curves';
import { ImageDimensions } from '../hooks/useImageRenderSize';
import { ToolType } from '../components/panel/right/Masks';
import { OverlayMode } from '../components/panel/right/CropPanel';
import { PROFILE_PROJECTION_NONE, type DevelopProfileProjection } from '../utils/developState';

interface InteractivePatch {
  url: string;
  normX: number;
  normY: number;
  normW: number;
  normH: number;
}

interface BaseRenderSize extends ImageDimensions {
  containerHeight: number;
  containerWidth: number;
  offsetX: number;
  offsetY: number;
}

/**
 * Snapshot of the profile transaction a same-image reload belongs to, so a
 * failed reload can put the previously rendered profile back.
 */
export interface XmpProfileRollback {
  /** The last successfully rendered profile view, restored if the reload fails. */
  profile: DevelopProfileProjection;
  id: number;
}

export { DEFAULT_XMP_PROFILE_AMOUNT_PERCENT } from '../utils/developState';

// Monotonic source of profile transaction ids; module-level so the store keeps
// no counter field of its own.
let xmpProfileTransactionId = 0;

/** Returns the next unique id for a single XMP profile transaction. */
export function nextXmpProfileTransactionId(): number {
  return ++xmpProfileTransactionId;
}

/**
 * Requests a same-image reload that applies (or clears) a profile.
 *
 * Single owner of the profile transaction: the picker, the amount slider and the
 * explicit XMP read all go through this action so the rollback/roll-forward
 * bookkeeping exists exactly once.
 */
export type RequestProfileTransaction = (profile: DevelopProfileProjection) => void;

interface EditorState {
  // Core Image & Adjustments
  selectedImage: SelectedImage | null;
  adjustments: Adjustments;
  previewOverride: Adjustments | null;

  // XMP Profile (session only, never persisted)
  xmpProfile: DevelopProfileProjection;
  /** Bumped ONLY when a profile transaction is requested. Drives the reload effect. */
  xmpProfileRevision: number;
  xmpProfileRollback: XmpProfileRollback | null;

  // History State
  history: Adjustments[];
  historyIndex: number;

  // Previews & Overlays
  finalPreviewUrl: string | null;
  uncroppedAdjustedPreviewUrl: string | null;
  interactivePatch: InteractivePatch | null;
  showOriginal: boolean;

  // Analytics
  histogram: ChannelConfig | null;
  waveform: WaveformData | null;
  isWaveformVisible: boolean;
  activeWaveformChannel: string;
  waveformHeight: number;

  // Interaction State
  isSliderDragging: boolean;
  zoom: number;
  displaySize: ImageDimensions;
  previewSize: ImageDimensions;
  baseRenderSize: BaseRenderSize;
  originalSize: ImageDimensions;

  // Tools State
  isRotationActive: boolean;
  overlayMode: OverlayMode;
  overlayRotation: number;
  isStraightenActive: boolean;
  isWbPickerActive: boolean;
  isGuidedPerspectiveActive: boolean;
  liveRotation: number | null;
  brushSettings: BrushSettings | null;

  // Masks & AI
  activeMaskContainerId: string | null;
  activeMaskId: string | null;
  activeAiPatchContainerId: string | null;
  activeAiSubMaskId: string | null;
  isMaskControlHovered: boolean;
  isGeneratingAiMask: boolean;
  isGeneratingAi: boolean;
  isAIConnectorConnected: boolean;
  hasRenderedFirstFrame: boolean;
  patchesSentToBackend: Set<string>;

  // Clipboard
  copiedSectionAdjustments: any | null;
  copiedMask: MaskContainer | null;
  copiedAdjustments: Adjustments | null;
  /**
   * The profile half of the clipboard. Kept beside `copiedAdjustments` rather
   * than folded into it: `'xmpProfile'` is a copy/paste marker, never a key of
   * `adjustments`, so the profile travels in a side-channel of its own.
   */
  copiedDevelopProfile: DevelopProfileProjection | null;

  // Actions
  setEditor: (updater: Partial<EditorState> | ((state: EditorState) => Partial<EditorState>)) => void;
  requestProfileTransaction: RequestProfileTransaction;
  pushHistory: (newAdjustments: Adjustments) => void;
  undo: () => void;
  redo: () => void;
  resetHistory: (initialState: Adjustments) => void;
  goToHistoryIndex: (index: number) => void;
}

export const useEditorStore = create<EditorState>((set, get) => ({
  selectedImage: null,
  adjustments: INITIAL_ADJUSTMENTS,
  previewOverride: null,
  xmpProfile: PROFILE_PROJECTION_NONE,
  xmpProfileRevision: 0,
  xmpProfileRollback: null,
  history: [INITIAL_ADJUSTMENTS],
  historyIndex: 0,

  finalPreviewUrl: null,
  uncroppedAdjustedPreviewUrl: null,
  showOriginal: false,
  histogram: null,
  waveform: null,
  isWaveformVisible: false,
  activeWaveformChannel: 'luma',
  waveformHeight: 220,

  isSliderDragging: false,
  interactivePatch: null,
  activeMaskContainerId: null,
  activeMaskId: null,
  activeAiPatchContainerId: null,
  activeAiSubMaskId: null,

  zoom: 1,
  displaySize: { width: 0, height: 0 },
  previewSize: { width: 0, height: 0 },
  baseRenderSize: { width: 0, height: 0, offsetX: 0, offsetY: 0, containerWidth: 0, containerHeight: 0 },
  originalSize: { width: 0, height: 0 },

  isRotationActive: false,
  overlayMode: 'thirds',
  overlayRotation: 0,
  isStraightenActive: false,
  isWbPickerActive: false,
  isGuidedPerspectiveActive: false,
  liveRotation: null,

  copiedSectionAdjustments: null,
  copiedMask: null,
  brushSettings: { size: 50, feather: 50, tool: ToolType.Brush },
  copiedAdjustments: null,
  copiedDevelopProfile: null,

  isGeneratingAiMask: false,
  isAIConnectorConnected: false,
  isGeneratingAi: false,
  isMaskControlHovered: false,
  hasRenderedFirstFrame: false,
  patchesSentToBackend: new Set<string>(),

  setEditor: (updater) => set((state) => (typeof updater === 'function' ? updater(state) : updater)),

  requestProfileTransaction: (profile) => {
    const { xmpProfile: currentProjection, xmpProfileRollback: pendingRollback, selectedImage: image } = get();

    // A reload for exactly this target is already in flight (isReady is false)
    // and this request would not change any reload input, so it has nothing left
    // to request. The in-flight transaction keeps ownership of the rollback:
    // rotating the transaction id here — with no reload to close it — would
    // orphan that rollback, so the next failure would restore stale fields.
    if (
      image?.isReady === false &&
      profile.status === currentProjection.status &&
      profile.path === currentProjection.path &&
      profile.name === currentProjection.name &&
      profile.uuid === currentProjection.uuid &&
      profile.amountPercent === currentProjection.amountPercent &&
      profile.supportsAmount === currentProjection.supportsAmount
    ) {
      return;
    }

    // An in-flight transaction's rollback is the last successfully rendered
    // state, so reuse its fields rather than snapshotting a not-yet-rendered
    // amount; a fresh object with a new id keeps each transaction's rollback
    // distinct so a superseded reload cannot clear this one.
    const base = pendingRollback ?? { profile: currentProjection };

    const id = nextXmpProfileTransactionId();

    set({
      xmpProfile: profile,
      xmpProfileRollback: { ...base, id },
      xmpProfileRevision: id,
      selectedImage: image ? { ...image, isReady: false } : image,
    });
  },

  pushHistory: (newAdj) =>
    set((state) => {
      const newHistory = state.history.slice(0, state.historyIndex + 1);
      newHistory.push(newAdj);
      if (newHistory.length > 50) newHistory.shift();
      return { history: newHistory, historyIndex: newHistory.length - 1 };
    }),

  undo: () =>
    set((state) => {
      if (state.historyIndex > 0) {
        const newIndex = state.historyIndex - 1;
        return { historyIndex: newIndex, adjustments: state.history[newIndex] };
      }
      return state;
    }),

  redo: () =>
    set((state) => {
      if (state.historyIndex < state.history.length - 1) {
        const newIndex = state.historyIndex + 1;
        return { historyIndex: newIndex, adjustments: state.history[newIndex] };
      }
      return state;
    }),

  resetHistory: (initialState) =>
    set({
      history: [initialState],
      historyIndex: 0,
      adjustments: initialState,
    }),

  goToHistoryIndex: (index) =>
    set((state) => {
      if (index >= 0 && index < state.history.length) {
        return { historyIndex: index, adjustments: state.history[index] };
      }
      return state;
    }),
}));
