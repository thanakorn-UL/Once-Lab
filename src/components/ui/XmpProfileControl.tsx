import { open } from '@tauri-apps/plugin-dialog';
import { invoke } from '@tauri-apps/api/core';
import { useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from 'react-toastify';
import debounce from 'lodash.debounce';
import { FileText, X } from 'lucide-react';
import { Invokes } from './AppProperties';
import Slider from './Slider';
import { useEditorStore, DEFAULT_XMP_PROFILE_AMOUNT_PERCENT, nextXmpProfileTransactionId } from '../../store/useEditorStore';

interface XmpProfileSummary {
  name: string;
  path: string;
  supports_amount: boolean;
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
  const xmpProfileAmountPercent = useEditorStore((s) => s.xmpProfileAmountPercent);
  const xmpProfileSupportsAmount = useEditorStore((s) => s.xmpProfileSupportsAmount);
  const setEditor = useEditorStore((s) => s.setEditor);
  const [isInspecting, setIsInspecting] = useState(false);

  // The slider is visually live while dragging, but only the released value is
  // committed, because committing re-develops the RAW file.
  const [draggedAmount, setDraggedAmount] = useState(xmpProfileAmountPercent);
  const draggedAmountRef = useRef(xmpProfileAmountPercent);
  const isDraggingRef = useRef(false);
  const isEditingRef = useRef(false);

  useEffect(() => {
    draggedAmountRef.current = xmpProfileAmountPercent;
    setDraggedAmount(xmpProfileAmountPercent);
  }, [xmpProfileAmountPercent]);

  const isRaw = Boolean(selectedImage?.isRaw);
  const isDisabled = !selectedImage || !isRaw || isInspecting;

  // Reloading the current RAW is what actually applies (or removes) a profile,
  // so the selection only becomes active once the reload has been requested.
  const commitProfile = (
    path: string | null,
    name: string | null,
    amountPercent: number = DEFAULT_XMP_PROFILE_AMOUNT_PERCENT,
    supportsAmount: boolean = false,
  ) => {
    const {
      xmpProfilePath,
      xmpProfileName: currentName,
      xmpProfileAmountPercent: currentAmount,
      xmpProfileSupportsAmount: currentSupportsAmount,
      xmpProfileRollback: pendingRollback,
      selectedImage: image,
    } = useEditorStore.getState();

    // A reload for exactly this target is already in flight (isReady is false)
    // and this commit would not change any reload input, so it has nothing left
    // to request. The in-flight transaction keeps ownership of the rollback:
    // rotating the transaction id here — with no reload to close it — would
    // orphan that rollback, so the next failure would restore stale fields.
    if (
      image?.isReady === false &&
      path === xmpProfilePath &&
      name === currentName &&
      amountPercent === currentAmount &&
      supportsAmount === currentSupportsAmount
    ) {
      return;
    }

    // An in-flight transaction's rollback is the last successfully rendered
    // state, so reuse its fields rather than snapshotting a not-yet-rendered
    // amount; a fresh object with a new id keeps each transaction's rollback
    // distinct so a superseded reload cannot clear this one.
    const base = pendingRollback ?? {
      path: xmpProfilePath,
      name: currentName,
      amountPercent: currentAmount,
      supportsAmount: currentSupportsAmount,
    };

    setEditor({
      xmpProfilePath: path,
      xmpProfileName: name,
      xmpProfileAmountPercent: amountPercent,
      xmpProfileSupportsAmount: supportsAmount,
      xmpProfileRollback: { ...base, id: nextXmpProfileTransactionId() },
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

      // A different profile always starts from its authored strength, so any
      // pending amount commit must not leak onto the newly chosen profile.
      debouncedAmountCommit.cancel();
      commitProfile(selection, summary.name, DEFAULT_XMP_PROFILE_AMOUNT_PERCENT, summary.supports_amount);
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
    commitProfile(null, null);
  };

  const handleAmountCommit = () => {
    const percent = draggedAmountRef.current;
    const {
      xmpProfilePath: path,
      xmpProfileName: name,
      xmpProfileAmountPercent: currentAmount,
    } = useEditorStore.getState();

    // The control only exists for an active profile that supports it.
    if (!path || percent === currentAmount) return;

    commitProfile(path, name, percent, true);
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

      {xmpProfileName && xmpProfileSupportsAmount && (
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
