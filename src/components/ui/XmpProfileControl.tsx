import { open } from '@tauri-apps/plugin-dialog';
import { invoke } from '@tauri-apps/api/core';
import { useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from 'react-toastify';
import debounce from 'lodash.debounce';
import { AlertTriangle, FileText, X } from 'lucide-react';
import { Invokes } from './AppProperties';
import Slider from './Slider';
import XmpProfileBrowser, { type XmpProfileEntry } from './XmpProfileBrowser';
import { useEditorStore, DEFAULT_XMP_PROFILE_AMOUNT_PERCENT } from '../../store/useEditorStore';
import { profileProjectionFromSelection } from '../../utils/developState';

interface XmpProfileSummary {
  name: string;
  path: string;
  uuid: string;
  supports_amount: boolean;
  convert_to_grayscale: boolean;
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
  const xmpProfile = useEditorStore((s) => s.xmpProfile);
  const requestProfileTransaction = useEditorStore((s) => s.requestProfileTransaction);
  const [isInspecting, setIsInspecting] = useState(false);

  // The slider is visually live while dragging, but only the released value is
  // committed, because committing re-develops the RAW file.
  const [draggedAmount, setDraggedAmount] = useState(xmpProfile.amountPercent);
  const draggedAmountRef = useRef(xmpProfile.amountPercent);
  const isDraggingRef = useRef(false);
  const isEditingRef = useRef(false);

  useEffect(() => {
    draggedAmountRef.current = xmpProfile.amountPercent;
    setDraggedAmount(xmpProfile.amountPercent);
  }, [xmpProfile.amountPercent]);

  const isRaw = Boolean(selectedImage?.isRaw);
  const isDisabled = !selectedImage || !isRaw || isInspecting;

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

      // A different profile always starts from its authored strength, so any
      // pending amount commit must not leak onto the newly chosen profile.
      debouncedAmountCommit.cancel();
      requestProfileTransaction(
        profileProjectionFromSelection(
          selection,
          summary.name,
          summary.uuid,
          DEFAULT_XMP_PROFILE_AMOUNT_PERCENT,
          summary.supports_amount,
        ),
      );
    } catch (err) {
      console.error('Failed to choose XMP profile:', err);
      toast.error(`${t('ui.xmpProfile.applyFailed')}: ${err}`);
    } finally {
      setIsInspecting(false);
    }
  };

  const handleClear = () => {
    // Clearing picks a new (empty) profile, so cancel any pending amount commit.
    debouncedAmountCommit.cancel();
    requestProfileTransaction(
      profileProjectionFromSelection(null, null, null, DEFAULT_XMP_PROFILE_AMOUNT_PERCENT, false),
    );
  };

  // A discovered entry already carries the metadata the backend parsed out of
  // the profile, so it feeds the very same transaction the manual picker uses
  // and never needs a second inspect round trip.
  const handleSelectDiscoveredProfile = (entry: XmpProfileEntry) => {
    debouncedAmountCommit.cancel();
    requestProfileTransaction(
      profileProjectionFromSelection(
        entry.path,
        entry.name,
        entry.uuid,
        DEFAULT_XMP_PROFILE_AMOUNT_PERCENT,
        entry.supports_amount,
      ),
    );
  };

  const handleAmountCommit = () => {
    const percent = draggedAmountRef.current;
    const { xmpProfile: projection } = useEditorStore.getState();

    // The control only exists for an active profile that supports it.
    if (projection.status !== 'resolved' || !projection.path || percent === projection.amountPercent) return;

    requestProfileTransaction(
      profileProjectionFromSelection(projection.path, projection.name, projection.uuid, percent, true),
    );
  };

  // Wheel and keyboard changes reach the control only through onChange, so they
  // are committed once the burst settles; a drag commits on release instead. The
  // stable identity keeps one pending timer alive across renders.
  const commitAmountRef = useRef(handleAmountCommit);
  commitAmountRef.current = handleAmountCommit;
  const debouncedAmountCommit = useMemo(() => debounce(() => commitAmountRef.current(), 300), []);

  useEffect(() => () => debouncedAmountCommit.cancel(), [debouncedAmountCommit]);

  return (
    <div className="mb-2">
      <div className="flex justify-between items-center">
        <span className="text-sm font-medium text-text-secondary select-none">{t('ui.xmpProfile.label')}</span>
        {xmpProfile.name && (
          <span className="flex min-w-0 items-center gap-1">
            {xmpProfile.status === 'missing' && (
              <AlertTriangle
                size={14}
                className="shrink-0"
                data-tooltip={t('ui.xmpProfile.unavailableRoot')}
                aria-label={t('ui.xmpProfile.unavailableRoot')}
              />
            )}
            <span className="truncate max-w-35 text-sm text-text-primary" data-tooltip={xmpProfile.name}>
              {xmpProfile.name}
            </span>
          </span>
        )}
      </div>

      <div className="flex items-center gap-2 mt-2">
        <button
          onClick={handleChoose}
          disabled={isDisabled}
          className="flex-1 flex items-center justify-center gap-1.5 py-1.5 rounded-md bg-bg-tertiary hover:bg-surface border border-surface text-sm text-text-primary transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
          data-tooltip={xmpProfile.name || t('ui.xmpProfile.choose')}
        >
          <FileText size={14} />
          {xmpProfile.name ? t('ui.xmpProfile.change') : t('ui.xmpProfile.choose')}
        </button>

        {xmpProfile.name && (
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

      <XmpProfileBrowser
        activePath={xmpProfile.path}
        applyDisabled={isDisabled}
        onSelectProfile={handleSelectDiscoveredProfile}
      />

      {xmpProfile.name && xmpProfile.supportsAmount && (
        <Slider
          label={t('ui.xmpProfile.amount')}
          min={0}
          max={200}
          step={1}
          value={draggedAmount}
          defaultValue={DEFAULT_XMP_PROFILE_AMOUNT_PERCENT}
          disabled={isDisabled}
          onChange={(e: any) => {
            const percent = Number(e.target.value);
            draggedAmountRef.current = percent;
            setDraggedAmount(percent);
            // A drag commits on release and a text edit commits on Enter/blur, so
            // only wheel and keyboard need the debounce to settle their burst.
            if (!isDraggingRef.current && !isEditingRef.current) debouncedAmountCommit();
          }}
          onDragStateChange={(state) => {
            isDraggingRef.current = state;
            // The mousedown click-jump emits onChange before the drag begins, so
            // cancel the debounce it scheduled; release or settle commits once.
            if (state) debouncedAmountCommit.cancel();
          }}
          onEditingChange={(state) => {
            isEditingRef.current = state;
            // Typing emits onChange on every keystroke but commits once at the end,
            // so cancel any debounce a keystroke may have scheduled.
            if (state) debouncedAmountCommit.cancel();
          }}
          onPointerUp={() => {
            debouncedAmountCommit.cancel();
            handleAmountCommit();
          }}
        />
      )}

      {selectedImage && !isRaw && (
        <p className="mt-2 text-xs text-text-secondary select-none">{t('ui.xmpProfile.rawOnly')}</p>
      )}
    </div>
  );
}
