import { open } from '@tauri-apps/plugin-dialog';
import { invoke } from '@tauri-apps/api/core';
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from 'react-toastify';
import { FileText, X } from 'lucide-react';
import { Invokes } from './AppProperties';
import { useEditorStore } from '../../store/useEditorStore';

interface XmpProfileSummary {
  name: string;
  path: string;
}

/**
 * Minimal XMP RGB profile picker for the current RAW image.
 *
 * The frontend only ever passes a file path to the backend; parsing and
 * application stay authoritative in Rust.
 */
export default function XmpProfileControl() {
  const { t } = useTranslation();
  const selectedImage = useEditorStore((s) => s.selectedImage);
  const xmpProfileName = useEditorStore((s) => s.xmpProfileName);
  const setEditor = useEditorStore((s) => s.setEditor);
  const [isInspecting, setIsInspecting] = useState(false);

  const isRaw = Boolean(selectedImage?.isRaw);
  const isDisabled = !selectedImage || !isRaw || isInspecting;

  // Reloading the current RAW is what actually applies (or removes) a profile,
  // so the selection only becomes active once the reload has been requested.
  const commitProfile = (path: string | null, name: string | null) => {
    const { xmpProfilePath, xmpProfileName: currentName, selectedImage: image } = useEditorStore.getState();

    setEditor({
      xmpProfilePath: path,
      xmpProfileName: name,
      xmpProfileRollback: { path: xmpProfilePath, name: currentName },
      selectedImage: image ? { ...image, isReady: false } : image,
    });
  };

  const handleChoose = async () => {
    try {
      const selection = await open({
        multiple: false,
        filters: [{ name: t('ui.xmpProfile.filterLabel'), extensions: ['xmp', 'XMP'] }],
      });

      // Cancelling the dialog must not reload or change any state.
      if (typeof selection !== 'string') return;

      setIsInspecting(true);

      let summary: XmpProfileSummary;
      try {
        summary = await invoke<XmpProfileSummary>(Invokes.InspectXmpProfile, { path: selection });
      } catch (err) {
        console.error('Failed to inspect XMP profile:', err);
        toast.error(`${t('ui.xmpProfile.applyFailed')}: ${err}`);
        return;
      }

      commitProfile(selection, summary.name);
    } catch (err) {
      console.error('Failed to choose XMP profile:', err);
      toast.error(`${t('ui.xmpProfile.applyFailed')}: ${err}`);
    } finally {
      setIsInspecting(false);
    }
  };

  const handleClear = () => {
    commitProfile(null, null);
  };

  return (
    <div className="mb-2">
      <div className="flex justify-between items-center">
        <span className="text-sm font-medium text-text-secondary select-none">{t('ui.xmpProfile.label')}</span>
        {xmpProfileName && (
          <span className="truncate max-w-35 text-sm text-text-primary" data-tooltip={xmpProfileName}>
            {xmpProfileName}
          </span>
        )}
      </div>

      <div className="flex items-center gap-2 mt-2">
        <button
          onClick={handleChoose}
          disabled={isDisabled}
          className="flex-1 flex items-center justify-center gap-1.5 py-1.5 rounded-md bg-bg-tertiary hover:bg-surface border border-surface text-sm text-text-primary transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
          data-tooltip={xmpProfileName || t('ui.xmpProfile.choose')}
        >
          <FileText size={14} />
          {xmpProfileName ? t('ui.xmpProfile.change') : t('ui.xmpProfile.choose')}
        </button>

        {xmpProfileName && (
          <button
            onClick={handleClear}
            disabled={isInspecting}
            className="flex items-center justify-center gap-1 px-2.5 py-1.5 rounded-md bg-bg-tertiary hover:bg-surface border border-surface text-sm text-text-secondary hover:text-text-primary transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
            data-tooltip={t('ui.xmpProfile.clear')}
          >
            <X size={14} />
            {t('ui.xmpProfile.clear')}
          </button>
        )}
      </div>

      {selectedImage && !isRaw && (
        <p className="mt-2 text-xs text-text-secondary select-none">{t('ui.xmpProfile.rawOnly')}</p>
      )}
    </div>
  );
}
