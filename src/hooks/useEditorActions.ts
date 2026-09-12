import { useCallback } from 'react';
import { invoke } from '@tauri-apps/api/core';
import debounce from 'lodash.debounce';
import { toast } from 'react-toastify';
import { useEditorStore } from '../store/useEditorStore';
import { useLibraryStore } from '../store/useLibraryStore';
import { useSettingsStore } from '../store/useSettingsStore';
import { useProcessStore } from '../store/useProcessStore';
import {
  Adjustments,
  INITIAL_ADJUSTMENTS,
  COPYABLE_ADJUSTMENT_KEYS,
  PasteMode,
  LensAdjustment,
  normalizeLoadedAdjustments,
} from '../utils/adjustments';
import { calculateCenteredCrop } from '../utils/cropUtils';
import {
  PROFILE_PROJECTION_NONE,
  developStateFromProfileFields,
  profileProjectionFromRestore,
  profileProjectionsEqual,
  type DevelopProfileProjection,
  type DevelopProfileRestore,
} from '../utils/developState';
import { Invokes } from '../components/ui/AppProperties';
import { globalImageCache } from '../utils/ImageLRUCache';

export const debouncedSetHistory = debounce((newAdj: Adjustments) => {
  useEditorStore.getState().pushHistory(newAdj);
}, 500);

export const debouncedSave = debounce((path: string, adjustmentsToSave: Adjustments) => {
  invoke(Invokes.SaveMetadataAndUpdateThumbnail, { path, adjustments: adjustmentsToSave }).catch((err) => {
    console.error('Auto-save failed:', err);
    toast.error(`Failed to save changes: ${err}`);
  });
}, 300);

export function useEditorActions() {
  const setEditor = useEditorStore((s) => s.setEditor);

  const setAdjustments = useCallback(
    (value: Partial<Adjustments> | ((prev: Adjustments) => Adjustments)) => {
      setEditor((state) => {
        const prev = state.adjustments;
        const newAdjustments = typeof value === 'function' ? value(prev) : { ...prev, ...value };
        debouncedSetHistory(newAdjustments);
        return {
          adjustments: newAdjustments,
          ...(state.showOriginal ? { showOriginal: false, previewOverride: null } : {}),
        };
      });
    },
    [setEditor],
  );

  const handleRotate = useCallback(
    (degrees: number) => {
      const { selectedImage, adjustments } = useEditorStore.getState();
      const increment = degrees > 0 ? 1 : 3;
      const newAspectRatio =
        adjustments.aspectRatio && adjustments.aspectRatio !== 0 ? 1 / adjustments.aspectRatio : null;
      const newOrientationSteps = ((adjustments.orientationSteps || 0) + increment) % 4;
      const newCrop =
        selectedImage?.width && selectedImage?.height
          ? calculateCenteredCrop(selectedImage.width, selectedImage.height, newOrientationSteps, newAspectRatio)
          : null;

      setAdjustments((prev) => ({
        ...prev,
        aspectRatio: newAspectRatio,
        orientationSteps: newOrientationSteps,
        rotation: 0,
        crop: newCrop,
      }));
    },
    [setAdjustments],
  );

  const handleAutoAdjustments = useCallback(async () => {
    const selectedImage = useEditorStore.getState().selectedImage;
    if (!selectedImage?.isReady) return;
    try {
      const autoAdjustments: Adjustments = await invoke(Invokes.CalculateAutoAdjustments);
      setAdjustments((prev: Adjustments) => ({
        ...prev,
        ...autoAdjustments,
        sectionVisibility: { ...prev.sectionVisibility, ...autoAdjustments.sectionVisibility },
      }));
    } catch (err) {
      toast.error(`Failed to apply auto adjustments: ${err}`);
    }
  }, [setAdjustments]);

  const toggleShowOriginal = useCallback(() => {
    setEditor((state) => {
      const isShowing = !state.showOriginal;

      if (isShowing) {
        const override = { ...INITIAL_ADJUSTMENTS };
        const geometryKeys: Array<keyof Adjustments> = [
          'crop',
          'rotation',
          'flipHorizontal',
          'flipVertical',
          'orientationSteps',
          'aspectRatio',
          'transformDistortion',
          'transformVertical',
          'transformHorizontal',
          'transformRotate',
          'transformAspect',
          'transformScale',
          'transformXOffset',
          'transformYOffset',
          'lensDistortionAmount',
          'lensVignetteAmount',
          'lensTcaAmount',
          'lensDistortionParams',
          'lensMaker',
          'lensModel',
          'lensDistortionEnabled',
          'lensTcaEnabled',
          'lensVignetteEnabled',
        ];

        geometryKeys.forEach((key) => {
          (override as any)[key] = state.adjustments[key];
        });

        return { showOriginal: true, previewOverride: override };
      } else {
        return { showOriginal: false, previewOverride: null };
      }
    });
  }, [setEditor]);

  const handleLutSelect = useCallback(
    async (path: string, isBuiltIn: boolean = false) => {
      const isAndroid = useSettingsStore.getState().osPlatform === 'android';
      try {
        const result: { size: number } = await invoke('load_and_parse_lut', { path });
        const name =
          isAndroid && path.startsWith('content://')
            ? await invoke<string>('resolve_android_content_uri_name', { uriStr: path })
            : path.split(/[\\/]/).pop() || 'LUT';
        setAdjustments((prev: Adjustments) => ({
          ...prev,
          lutPath: path,
          lutName: name,
          lutSize: result.size,
          lutIntensity: 100,
          lutIsSceneReferred: isBuiltIn,
          sectionVisibility: {
            ...(prev.sectionVisibility || INITIAL_ADJUSTMENTS.sectionVisibility),
            effects: true,
          },
        }));
      } catch (err) {
        toast.error(`Failed to load LUT: ${err}`);
      }
    },
    [setAdjustments],
  );

  const setLutPreviewOverride = useCallback(
    (path: string | null, isBuiltIn: boolean = false) => {
      setEditor((state) => {
        if (!path) return { previewOverride: null };
        const name = path.split(/[\\/]/).pop() || 'LUT';
        return {
          previewOverride: {
            ...state.adjustments,
            lutPath: path,
            lutName: name,
            lutIntensity: state.adjustments.lutIntensity,
            lutIsSceneReferred: isBuiltIn,
          },
        };
      });
    },
    [setEditor],
  );

  const handleResetAdjustments = useCallback(
    (paths?: string[]) => {
      const { multiSelectedPaths, libraryActivePath, setLibrary } = useLibraryStore.getState();
      const { selectedImage, resetHistory } = useEditorStore.getState();
      const pathsToReset = paths || multiSelectedPaths;
      if (pathsToReset.length === 0) return;

      pathsToReset.forEach((p) => globalImageCache.delete(p));
      debouncedSetHistory.cancel();

      invoke(Invokes.ResetAdjustmentsForPaths, { paths: pathsToReset })
        .then(() => {
          if (libraryActivePath && pathsToReset.includes(libraryActivePath))
            setLibrary({ libraryActiveAdjustments: { ...INITIAL_ADJUSTMENTS } });
          if (selectedImage && pathsToReset.includes(selectedImage.path)) {
            const aspect =
              selectedImage.width && selectedImage.height ? selectedImage.width / selectedImage.height : null;
            const resetData = { ...INITIAL_ADJUSTMENTS, aspectRatio: aspect, aiPatches: [] };
            resetHistory(resetData);
            setEditor({ adjustments: resetData });
          }
        })
        .catch((err) => toast.error(`Failed to reset adjustments: ${err}`));
    },
    [setEditor],
  );

  const handleAutoLensCorrection = useCallback(
    (paths?: string[]) => {
      const { multiSelectedPaths, libraryActivePath, setLibrary } = useLibraryStore.getState();
      const { selectedImage, resetHistory } = useEditorStore.getState();

      const pathsToUpdate =
        paths && paths.length > 0
          ? paths
          : multiSelectedPaths.length > 0
            ? multiSelectedPaths
            : selectedImage
              ? [selectedImage.path]
              : [];

      if (pathsToUpdate.length === 0) return;

      pathsToUpdate.forEach((p) => globalImageCache.delete(p));

      invoke('apply_auto_lens_correction_to_paths', { paths: pathsToUpdate })
        .then(async () => {
          if (selectedImage && pathsToUpdate.includes(selectedImage.path)) {
            const meta: any = await invoke(Invokes.LoadMetadata, { path: selectedImage.path });
            if (meta.adjustments && !meta.adjustments.is_null) {
              const normalized = normalizeLoadedAdjustments(meta.adjustments);
              setEditor({ adjustments: normalized });
              resetHistory(normalized);
            }
          }
          if (libraryActivePath && pathsToUpdate.includes(libraryActivePath)) {
            const meta: any = await invoke(Invokes.LoadMetadata, { path: libraryActivePath });
            if (meta.adjustments && !meta.adjustments.is_null) {
              setLibrary({ libraryActiveAdjustments: normalizeLoadedAdjustments(meta.adjustments) });
            }
          }
        })
        .catch((err) => toast.error(`Failed to apply auto lens correction: ${err}`));
    },
    [setEditor],
  );

  const handleCopyAdjustments = useCallback(async (pathOrEvent?: string | any) => {
    const pathOverride = typeof pathOrEvent === 'string' ? pathOrEvent : undefined;
    const { selectedImage, adjustments } = useEditorStore.getState();
    const { libraryActivePath, multiSelectedPaths } = useLibraryStore.getState();
    let sourceAdjustments: any = null;
    // The profile half of the clipboard. Copy is read-only, and the include /
    // exclude decision is a PASTE-time concern exactly as it is for every key.
    let sourceProfile: DevelopProfileProjection = PROFILE_PROJECTION_NONE;

    const pathToCopyFrom =
      pathOverride || (selectedImage ? selectedImage.path : libraryActivePath || multiSelectedPaths[0]);

    if (selectedImage && pathToCopyFrom === selectedImage.path) {
      sourceAdjustments = adjustments;
      // The open image's profile is already in the store; no extra round trip.
      sourceProfile = useEditorStore.getState().xmpProfile;
    } else if (pathToCopyFrom) {
      try {
        const meta: any = await invoke(Invokes.LoadMetadata, { path: pathToCopyFrom });
        if (meta?.adjustments && !meta.adjustments.is_null) {
          sourceAdjustments = normalizeLoadedAdjustments(meta.adjustments);
        } else {
          sourceAdjustments = INITIAL_ADJUSTMENTS;
        }

        // A source that is not the open image has no store projection, so its
        // profile comes from the same restore command 6C2's reload resolves with.
        const restore = await invoke<DevelopProfileRestore>(Invokes.ResolveImageDevelopProfile, {
          path: pathToCopyFrom,
        });
        sourceProfile = profileProjectionFromRestore(restore);
      } catch (err) {
        toast.error(`Failed to load metadata for copying: ${err}`);
        return;
      }
    }

    if (!sourceAdjustments) return;

    const adjustmentsToCopy: any = {};

    for (const key of COPYABLE_ADJUSTMENT_KEYS) {
      if (
        Object.prototype.hasOwnProperty.call(sourceAdjustments, key) &&
        Object.prototype.hasOwnProperty.call(INITIAL_ADJUSTMENTS, key)
      ) {
        adjustmentsToCopy[key] = structuredClone(sourceAdjustments[key]);
      }
    }
    useEditorStore.getState().setEditor({ copiedAdjustments: adjustmentsToCopy, copiedDevelopProfile: sourceProfile });
    useProcessStore.getState().setProcess({ isCopied: true });
  }, []);

  const handlePasteAdjustments = useCallback(
    (paths?: string[]) => {
      const { copiedAdjustments, selectedImage, adjustments } = useEditorStore.getState();
      const { multiSelectedPaths } = useLibraryStore.getState();
      const { appSettings } = useSettingsStore.getState();
      const { setProcess } = useProcessStore.getState();

      if (!copiedAdjustments || !appSettings) return;

      const { mode, includedAdjustments } = appSettings.copyPasteSettings;
      const adjustmentsToApply: Partial<Adjustments> = {};

      for (const key of includedAdjustments) {
        if (Object.prototype.hasOwnProperty.call(copiedAdjustments, key)) {
          const value = copiedAdjustments[key as keyof Adjustments];
          if (mode === PasteMode.Merge) {
            const defaultValue = INITIAL_ADJUSTMENTS[key as keyof Adjustments];
            if (JSON.stringify(value) !== JSON.stringify(defaultValue))
              adjustmentsToApply[key as keyof Adjustments] = value;
          } else {
            adjustmentsToApply[key as keyof Adjustments] = value;
          }
        }
      }

      if (includedAdjustments.includes(LensAdjustment.LensMaker)) {
        if (!adjustmentsToApply.lensMaker) {
          adjustmentsToApply.lensDistortionParams = null;
        }
      }

      // The profile is a copy/paste category of its own: `'xmpProfile'` is a
      // MARKER in `includedAdjustments` and is never a property of `adjustments`,
      // so the delta above can never carry it. R5: Merge applies the copied
      // profile only when something was actually copied (status other than
      // `none`); Replace always applies, including a clear.
      const includesProfile = includedAdjustments.includes('xmpProfile');
      const copiedProfile = useEditorStore.getState().copiedDevelopProfile;
      const shouldWriteProfile =
        includesProfile && copiedProfile !== null && (mode !== PasteMode.Merge || copiedProfile.status !== 'none');
      const profilePayload =
        shouldWriteProfile && copiedProfile
          ? developStateFromProfileFields({
              path: copiedProfile.path,
              name: copiedProfile.name,
              uuid: copiedProfile.uuid,
              amountPercent: copiedProfile.amountPercent,
            }).profile
          : undefined; // undefined = do not touch any target's profile

      const hasAdjustmentsDelta = Object.keys(adjustmentsToApply).length > 0;

      // A paste may now change ONLY the profile, so the empty-delta early return
      // may not skip the profile write. It still means "nothing was written" when
      // neither half has anything to do.
      if (!hasAdjustmentsDelta && !shouldWriteProfile) {
        setProcess({ isPasted: true });
        return;
      }

      const pathsToUpdate =
        paths || (multiSelectedPaths.length > 0 ? multiSelectedPaths : selectedImage ? [selectedImage.path] : []);
      if (pathsToUpdate.length === 0) return;

      pathsToUpdate.forEach((p) => globalImageCache.delete(p));

      // The open image's profile is driven through the transaction owner below,
      // never by a batch write, so its own call never carries a profile change.
      // Its adjustments still go through the backend (the lens database resolves
      // there) and are read back afterwards.
      const openImagePath = selectedImage && pathsToUpdate.includes(selectedImage.path) ? selectedImage.path : null;
      const profileTargets = openImagePath ? pathsToUpdate.filter((p) => p !== openImagePath) : pathsToUpdate;

      if (hasAdjustmentsDelta && openImagePath) {
        setAdjustments({ ...adjustments, ...adjustmentsToApply });
      }

      // The open image's adjustments must go through the backend too (that is where
      // the lens database resolves `lensDistortionParams`), and the resolved values
      // are read back into the store below. It is kept in its OWN call so the two
      // writes never share a path: disjoint sets cannot race, and every target is
      // still written exactly once.
      if (hasAdjustmentsDelta && openImagePath) {
        invoke(Invokes.ApplyDevelopStateToPaths, {
          paths: [openImagePath],
          adjustments: adjustmentsToApply,
          profile: { action: 'keep' },
        })
          .then(() => {
            invoke('load_metadata', { path: openImagePath }).then((meta: any) => {
              if (!meta.adjustments) return;
              setAdjustments((prev: any) => ({
                ...prev,
                lensMaker: meta.adjustments.lensMaker,
                lensModel: meta.adjustments.lensModel,
                lensDistortionParams: meta.adjustments.lensDistortionParams,
              }));
            });
          })
          .catch((err) => toast.error(`Failed to paste adjustments: ${err}`));
      }

      // The other targets get the adjustments and the profile in ONE write.
      if (profileTargets.length > 0) {
        invoke(Invokes.ApplyDevelopStateToPaths, {
          paths: profileTargets,
          adjustments: hasAdjustmentsDelta ? adjustmentsToApply : null,
          profile: shouldWriteProfile ? { action: 'set', profile: profilePayload } : { action: 'keep' },
        }).catch((err) => console.error('Failed to paste develop state to the selection:', err));
      }

      if (shouldWriteProfile && openImagePath && copiedProfile) {
        // Never `setEditor({ xmpProfile })`: the single transaction owner drives
        // the reload and persists the committed profile once on success. An
        // unchanged projection needs no reload at all. `copiedProfile` already IS
        // a `DevelopProfileProjection`, so it is passed straight through.
        if (!profileProjectionsEqual(useEditorStore.getState().xmpProfile, copiedProfile)) {
          useEditorStore.getState().requestProfileTransaction(copiedProfile);
        }
      }

      setProcess({ isPasted: true });
    },
    [setAdjustments],
  );

  const handleZoomChange = useCallback((zoomValue: number, fitToWindow: boolean = false) => {
    const { originalSize, baseRenderSize, adjustments } = useEditorStore.getState();
    const dpr = typeof window !== 'undefined' ? window.devicePixelRatio || 1 : 1;
    let targetZoomPercent: number;

    const orientationSteps = adjustments.orientationSteps || 0;
    const isSwapped = orientationSteps === 1 || orientationSteps === 3;
    const effectiveOriginalWidth = isSwapped ? originalSize.height : originalSize.width;
    const effectiveOriginalHeight = isSwapped ? originalSize.width : originalSize.height;

    if (fitToWindow) {
      if (
        effectiveOriginalWidth > 0 &&
        effectiveOriginalHeight > 0 &&
        baseRenderSize.width > 0 &&
        baseRenderSize.height > 0
      ) {
        const originalAspect = effectiveOriginalWidth / effectiveOriginalHeight;
        const baseAspect = baseRenderSize.width / baseRenderSize.height;
        targetZoomPercent =
          originalAspect > baseAspect
            ? baseRenderSize.width / effectiveOriginalWidth
            : baseRenderSize.height / effectiveOriginalHeight;
      } else {
        targetZoomPercent = 1.0;
      }
    } else {
      targetZoomPercent = zoomValue / dpr;
    }

    targetZoomPercent = Math.max(0.1 / dpr, Math.min(2.0, targetZoomPercent));

    let transformZoom = 1.0;
    if (
      effectiveOriginalWidth > 0 &&
      effectiveOriginalHeight > 0 &&
      baseRenderSize.width > 0 &&
      baseRenderSize.height > 0
    ) {
      const originalAspect = effectiveOriginalWidth / effectiveOriginalHeight;
      const baseAspect = baseRenderSize.width / baseRenderSize.height;
      if (originalAspect > baseAspect) {
        transformZoom = (targetZoomPercent * effectiveOriginalWidth) / baseRenderSize.width;
      } else {
        transformZoom = (targetZoomPercent * effectiveOriginalHeight) / baseRenderSize.height;
      }
    }
    useEditorStore.getState().setEditor({ zoom: transformZoom });
  }, []);

  return {
    setAdjustments,
    handleRotate,
    handleAutoAdjustments,
    handleLutSelect,
    setLutPreviewOverride,
    handleResetAdjustments,
    handleAutoLensCorrection,
    handleCopyAdjustments,
    handlePasteAdjustments,
    handleZoomChange,
    toggleShowOriginal,
  };
}
