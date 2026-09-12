import { invoke } from '@tauri-apps/api/core';
import { useCallback } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from 'react-toastify';
import { Invokes } from '../components/ui/AppProperties';
import { useEditorStore } from '../store/useEditorStore';
import { globalImageCache } from '../utils/ImageLRUCache';
import {
  developStateFromProfileFields,
  profileProjectionFromRestore,
  readXmpOutcomeRestore,
} from '../utils/developState';

/**
 * The explicit, user-initiated XMP develop-state actions, shared by every
 * context menu that offers them.
 *
 * Neither action is ever automatic: "Write Metadata to XMP" takes the current
 * internal state, and "Read Metadata from XMP" replaces it for exactly the paths
 * the user asked about. Both run per path so one failure never aborts the rest.
 */
export function useDevelopMetadataActions(): {
  writeDevelopMetadataToXmp: (paths: string[]) => Promise<void>;
  readDevelopMetadataFromXmp: (paths: string[]) => Promise<void>;
} {
  const { t } = useTranslation();

  const writeDevelopMetadataToXmp = useCallback(
    async (paths: string[]) => {
      for (const path of paths) {
        try {
          await invoke(Invokes.WriteDevelopStateToXmp, { path });
        } catch (err) {
          console.error('Failed to write develop metadata to XMP:', err);
          toast.error(`${t('contextMenus.editor.writeMetadataToXmp')}: ${err}`);
        }
      }
    },
    [t],
  );

  const readDevelopMetadataFromXmp = useCallback(
    async (paths: string[]) => {
      for (const path of paths) {
        try {
          const outcome = await invoke(Invokes.ReadDevelopStateFromXmp, { path });
          const restore = readXmpOutcomeRestore(outcome);
          if (!restore) {
            // Report it in the same `${action}: ${err}` shape as the catch block
            // below, so a user can always tell which action failed.
            const err = `the XMP read for ${path} did not return a develop-state payload`;
            console.error('Failed to read develop metadata from XMP:', err);
            toast.error(`${t('contextMenus.editor.readMetadataFromXmp')}: ${err}`);
            continue;
          }

          const projection = profileProjectionFromRestore(restore);

          // The open image already has a transaction owner, and its success branch
          // persists the internal state exactly once. Routing through it here and
          // NOT calling SaveImageDevelopProfile as well is what keeps a read of the
          // open image from double-writing.
          if (useEditorStore.getState().selectedImage?.path === path) {
            useEditorStore.getState().requestProfileTransaction(projection);
            continue;
          }

          // Any other path has no open transaction to drive the reload, so the
          // internal state is persisted directly; dropping the cached projection
          // stops a stale one from winning the next time the image is viewed.
          await invoke(Invokes.SaveImageDevelopProfile, {
            path,
            profile: developStateFromProfileFields({
              path: projection.path,
              name: projection.name,
              uuid: projection.uuid,
              amountPercent: projection.amountPercent,
            }).profile,
          });
          globalImageCache.delete(path);
        } catch (err) {
          console.error('Failed to read develop metadata from XMP:', err);
          toast.error(`${t('contextMenus.editor.readMetadataFromXmp')}: ${err}`);
        }
      }
    },
    [t],
  );

  return { writeDevelopMetadataToXmp, readDevelopMetadataFromXmp };
}
