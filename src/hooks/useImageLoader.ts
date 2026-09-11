import { useEffect, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { toast } from 'react-toastify';
import { useEditorStore } from '../store/useEditorStore';
import { useLibraryStore } from '../store/useLibraryStore';
import { useSettingsStore } from '../store/useSettingsStore';
import { Invokes } from '../components/ui/AppProperties';
import { INITIAL_ADJUSTMENTS, normalizeLoadedAdjustments } from '../utils/adjustments';

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
  const xmpProfilePath = useEditorStore((s) => s.xmpProfilePath);
  const xmpProfileAmountPercent = useEditorStore((s) => s.xmpProfileAmountPercent);

  // Tracks the image whose sidecar metadata has already been applied, so that a
  // profile-triggered reload of the same image keeps the user's adjustments.
  const loadedMetadataPathRef = useRef<string | null>(null);

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

          // Reloading the same image to change its XMP profile must not reset
          // the current adjustments or the undo history.
          if (loadedMetadataPathRef.current === selectedImage.path) return;

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
          loadedMetadataPathRef.current = selectedImage.path;
        } catch (err) {
          console.error('Failed to load metadata early:', err);
        }
      };

      const loadFullImageData = async () => {
        try {
          const {
            xmpProfilePath: profilePath,
            xmpProfileAmountPercent,
            xmpProfileSupportsAmount,
          } = useEditorStore.getState();
          const loadImageResult: any = profilePath
            ? await invoke(Invokes.LoadImageWithXmpProfile, {
                path: selectedImage.path,
                xmpProfilePath: profilePath,
                // A profile that does not support an amount hard-rejects any
                // override, so only the raw percent of a supporting profile is sent.
                profileAmountPercent: xmpProfileSupportsAmount ? xmpProfileAmountPercent : null,
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
                  selectedImage: state.selectedImage
                    ? { ...state.selectedImage, isReady: true }
                    : state.selectedImage,
                  xmpProfilePath: profileRollback.path,
                  xmpProfileName: profileRollback.name,
                  xmpProfileAmountPercent: profileRollback.amountPercent,
                  xmpProfileSupportsAmount: profileRollback.supportsAmount,
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
    xmpProfilePath,
    xmpProfileAmountPercent,
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
