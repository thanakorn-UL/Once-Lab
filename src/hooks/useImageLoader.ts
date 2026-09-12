import { useEffect } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { toast } from 'react-toastify';
import { useTranslation } from 'react-i18next';
import { useEditorStore } from '../store/useEditorStore';
import { useLibraryStore } from '../store/useLibraryStore';
import { useSettingsStore } from '../store/useSettingsStore';
import { Invokes } from '../components/ui/AppProperties';
import { INITIAL_ADJUSTMENTS, normalizeLoadedAdjustments } from '../utils/adjustments';
import { globalImageCache } from '../utils/ImageLRUCache';
import {
  PROFILE_PROJECTION_NONE,
  activeProfilePath,
  developStateFromProfileFields,
  profileProjectionFromRestore,
  type DevelopProfileRestore,
} from '../utils/developState';

export function useImageLoader(cachedEditStateRef: React.RefObject<any>) {
  const selectedImage = useEditorStore((s) => s.selectedImage);
  const adjustments = useEditorStore((s) => s.adjustments);
  const histogram = useEditorStore((s) => s.histogram);
  const waveform = useEditorStore((s) => s.waveform);
  const finalPreviewUrl = useEditorStore((s) => s.finalPreviewUrl);
  const uncroppedAdjustedPreviewUrl = useEditorStore((s) => s.uncroppedAdjustedPreviewUrl);
  const originalSize = useEditorStore((s) => s.originalSize);
  const previewSize = useEditorStore((s) => s.previewSize);
  const hasRenderedFirstFrame = useEditorStore((s) => s.hasRenderedFirstFrame);

  const setEditor = useEditorStore((s) => s.setEditor);
  const resetHistory = useEditorStore((s) => s.resetHistory);
  const setLibrary = useLibraryStore((s) => s.setLibrary);
  const appSettings = useSettingsStore((s) => s.appSettings);
  const xmpProfileRevision = useEditorStore((s) => s.xmpProfileRevision);
  const { t } = useTranslation();

  const isWgpuActive = appSettings?.useWgpuRenderer !== false && selectedImage?.isReady && hasRenderedFirstFrame;

  useEffect(() => {
    if (selectedImage && !selectedImage.isReady && selectedImage.path) {
      let isEffectActive = true;

      // Snapshot of the profile transaction this reload belongs to, taken when the
      // reload is requested. A non-null rollback means this load is the same-image
      // reload that applies (or clears) an XMP profile, so a failure must put the
      // previous profile back instead of closing the image that is already open.
      const { xmpProfileRollback: profileRollback } = useEditorStore.getState();

      const loadMetadataEarly = async () => {
        try {
          useEditorStore.getState().patchesSentToBackend.clear();
          await invoke('clear_session_caches').catch((e) => console.warn('Cache clear failed:', e));

          // Reloading the still-open image to change its XMP profile must not reset the
          // current adjustments or the undo history. A pending profile transaction is the
          // exact signal: it is set together with isReady=false and cleared once the reload
          // resolves. A reload with no transaction (a freshly opened image, or a cache entry
          // that disk has since contradicted) must always re-apply the sidecar metadata.
          if (useEditorStore.getState().xmpProfileRollback !== null) return;

          const metadata: any = await invoke(Invokes.LoadMetadata, { path: selectedImage.path });
          if (!isEffectActive) return;

          let initialAdjusts;
          if (metadata.adjustments && !metadata.adjustments.is_null) {
            initialAdjusts = normalizeLoadedAdjustments(metadata.adjustments);
          } else {
            initialAdjusts = { ...INITIAL_ADJUSTMENTS };
          }

          setEditor({ adjustments: initialAdjusts });
          resetHistory(initialAdjusts);

          try {
            const restore = await invoke<DevelopProfileRestore>(Invokes.ResolveImageDevelopProfile, {
              path: selectedImage.path,
            });
            if (!isEffectActive) return;
            setEditor({ xmpProfile: profileProjectionFromRestore(restore) });
          } catch (err) {
            console.error('Failed to restore develop profile:', err);
            setEditor({ xmpProfile: PROFILE_PROJECTION_NONE });
          }
        } catch (err) {
          console.error('Failed to load metadata early:', err);
        }
      };

      const loadFullImageData = async () => {
        try {
          const { xmpProfile } = useEditorStore.getState();
          const profilePath = activeProfilePath(xmpProfile);
          const loadImageResult: any = profilePath
            ? await invoke(Invokes.LoadImageWithXmpProfile, {
                path: selectedImage.path,
                xmpProfilePath: profilePath,
                // A profile that does not support an amount hard-rejects any
                // override, so only the raw percent of a supporting profile is sent.
                profileAmountPercent: xmpProfile.supportsAmount ? xmpProfile.amountPercent : null,
              })
            : await invoke(Invokes.LoadImage, { path: selectedImage.path });
          if (!isEffectActive) return;

          const { width, height } = loadImageResult;
          setEditor({ originalSize: { width, height } });

          if (appSettings?.editorPreviewResolution) {
            const maxSize = appSettings.editorPreviewResolution;
            const aspectRatio = width / height;

            if (width > height) {
              const pWidth = Math.min(width, maxSize);
              const pHeight = Math.round(pWidth / aspectRatio);
              setEditor({ previewSize: { width: pWidth, height: pHeight } });
            } else {
              const pHeight = Math.min(height, maxSize);
              const pWidth = Math.round(pHeight * aspectRatio);
              setEditor({ previewSize: { width: pWidth, height: pHeight } });
            }
          } else {
            setEditor({ previewSize: { width: 0, height: 0 } });
          }

          setEditor((state) => {
            if (state.selectedImage && state.selectedImage.path === selectedImage.path) {
              return {
                selectedImage: {
                  ...state.selectedImage,
                  exif: loadImageResult.exif,
                  height: loadImageResult.height,
                  isRaw: loadImageResult.is_raw,
                  isReady: true,
                  metadata: loadImageResult.metadata,
                  width: loadImageResult.width,
                },
              };
            }
            return state;
          });

          setEditor((state) => {
            if (!state.adjustments.aspectRatio && !state.adjustments.crop) {
              return {
                adjustments: { ...state.adjustments, aspectRatio: loadImageResult.width / loadImageResult.height },
              };
            }
            return state;
          });

          // The reload carried the profile change, so the optimistic transaction
          // is closed: the committed profile is the one now rendered. Clearing is
          // guarded by the transaction id so a superseded request cannot wipe the
          // rollback a newer reload is relying on.
          if (profileRollback) {
            const shouldPersist = useEditorStore.getState().xmpProfileRollback?.id === profileRollback.id;
            if (shouldPersist) {
              const profile = useEditorStore.getState().xmpProfile;
              const payload = developStateFromProfileFields({
                path: profile.path,
                name: profile.name,
                uuid: profile.uuid,
                amountPercent: profile.amountPercent,
              }).profile;

              invoke(Invokes.SaveImageDevelopProfile, {
                path: selectedImage.path,
                profile: payload,
              }).catch((err) => {
                console.error('Failed to save develop profile:', err);
                toast.error(`${t('ui.xmpProfile.applyFailed')}: ${err}`);
              });

              // 6C4-R6: the existing autoSync setting IS the multi-image sync path.
              // A committed profile change propagates to the rest of the selection
              // through the same batch command the paste uses; a failed target is
              // logged, never toasted per target.
              const { copyPasteSettings } = useSettingsStore.getState().appSettings ?? {};
              const otherPaths = useLibraryStore.getState().multiSelectedPaths.filter((p) => p !== selectedImage.path);

              if (
                copyPasteSettings?.autoSync &&
                (copyPasteSettings.includedAdjustments ?? []).includes('xmpProfile') &&
                otherPaths.length > 0
              ) {
                otherPaths.forEach((p) => globalImageCache.delete(p));
                invoke(Invokes.SaveDevelopProfileForPaths, { paths: otherPaths, profile: payload }).catch((err) => {
                  console.error('Failed to sync develop profile to the selection:', err);
                });
              }
            }
            setEditor((state) =>
              state.xmpProfileRollback?.id === profileRollback.id ? { ...state, xmpProfileRollback: null } : state,
            );
          }
        } catch (err) {
          if (isEffectActive) {
            console.error('Failed to load image:', err);
            toast.error(`Failed to load image: ${err}`);

            if (profileRollback) {
              // Changing a profile reloads the image that is already open, so a
              // failed attempt must keep it selected and put the previous profile
              // back rather than closing the image with the failed one committed.
              // Only the owning transaction may restore: if a newer commit has
              // superseded this one, that commit owns the state and resolves its
              // own outcome, so leave the store completely untouched here.
              setEditor((state) => {
                if (state.xmpProfileRollback?.id !== profileRollback.id) return state;
                return {
                  selectedImage: state.selectedImage ? { ...state.selectedImage, isReady: true } : state.selectedImage,
                  xmpProfile: profileRollback.profile,
                  xmpProfileRollback: null,
                };
              });
            } else {
              setEditor({ selectedImage: null });
            }
          }
        } finally {
          if (isEffectActive) {
            setLibrary({ isViewLoading: false });
          }
        }
      };

      const loadAll = async () => {
        await loadMetadataEarly();
        if (isEffectActive) {
          await loadFullImageData();
        }
      };

      loadAll();

      return () => {
        isEffectActive = false;
      };
    }
  }, [
    selectedImage?.path,
    selectedImage?.isReady,
    xmpProfileRevision,
    appSettings?.editorPreviewResolution,
    resetHistory,
    setEditor,
    setLibrary,
  ]);

  useEffect(() => {
    if (selectedImage?.path && selectedImage.isReady && (finalPreviewUrl || isWgpuActive)) {
      cachedEditStateRef.current = {
        adjustments,
        histogram,
        waveform,
        finalPreviewUrl,
        uncroppedPreviewUrl: uncroppedAdjustedPreviewUrl,
        selectedImage,
        originalSize,
        previewSize,
        developProfile: useEditorStore.getState().xmpProfile,
      };
    } else {
      cachedEditStateRef.current = null;
    }
  }, [
    selectedImage,
    adjustments,
    histogram,
    waveform,
    finalPreviewUrl,
    uncroppedAdjustedPreviewUrl,
    originalSize,
    previewSize,
    isWgpuActive,
    cachedEditStateRef,
  ]);
}
