import { invoke } from '@tauri-apps/api/core';
import { open } from '@tauri-apps/plugin-dialog';
import { useCallback, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from 'react-toastify';
import { AnimatePresence, motion } from 'framer-motion';
import { ChevronDown, FolderOpen, Loader2, RefreshCw } from 'lucide-react';
import { Invokes } from './AppProperties';

/**
 * One supported XMP RGB profile as returned by the discovery backend.
 *
 * The frontend never parses XMP: every field is authoritative backend output.
 * `path` is the profile's identity, because discovery deliberately keeps two
 * files that happen to share a `uuid`.
 */
export interface XmpProfileEntry {
  name: string;
  group: string | null;
  uuid: string;
  path: string;
  supports_amount: boolean;
}

interface ProfileGroup {
  /** `null` for profiles that carry no embedded group. */
  group: string | null;
  entries: XmpProfileEntry[];
}

interface XmpProfileBrowserProps {
  /** Path of the profile that is currently applied, for the active row. */
  activePath: string | null;
  /** True when applying is unavailable, e.g. while a non-RAW image is open. */
  applyDisabled: boolean;
  onSelectProfile: (entry: XmpProfileEntry) => void;
}

/**
 * Minimal profile library browser: choose one folder, list the supported
 * profiles it contains, and hand a chosen entry to the parent.
 *
 * Applying a profile stays entirely with the parent, so the browser never
 * touches the profile transaction, the Amount control or image readiness.
 * Nothing here is persisted: the chosen root lives for the session only.
 */
export default function XmpProfileBrowser({ activePath, applyDisabled, onSelectProfile }: XmpProfileBrowserProps) {
  const { t } = useTranslation();

  const [isExpanded, setIsExpanded] = useState(false);
  const [root, setRoot] = useState<string | null>(null);
  const [entries, setEntries] = useState<XmpProfileEntry[]>([]);
  const [isLoading, setIsLoading] = useState(false);

  // Only a successful scan replaces the library, so a failed root keeps the
  // profiles the user was already browsing.
  const discover = useCallback(
    async (nextRoot: string) => {
      setIsLoading(true);
      try {
        const discovered = await invoke<XmpProfileEntry[]>(Invokes.DiscoverXmpProfiles, { roots: [nextRoot] });
        setRoot(nextRoot);
        setEntries(discovered);
      } catch (err) {
        console.error('Failed to discover XMP profiles:', err);
        toast.error(`${t('ui.xmpProfile.browseFailed')}: ${err}`);
      } finally {
        setIsLoading(false);
      }
    },
    [t],
  );

  const handleChooseFolder = async () => {
    try {
      const selection = await open({ directory: true, multiple: false });

      // Cancelling the dialog must not scan, error or change any state.
      if (typeof selection !== 'string') return;

      await discover(selection);
    } catch (err) {
      console.error('Failed to choose profile folder:', err);
      toast.error(`${t('ui.xmpProfile.browseFailed')}: ${err}`);
    }
  };

  // One first-appearance pass is enough: the backend already ordered profiles
  // by group and name, so this preserves its deterministic order and only
  // brackets runs of the same group.
  const groups = useMemo(() => {
    const grouped: ProfileGroup[] = [];
    const buckets = new Map<string, ProfileGroup>();

    for (const entry of entries) {
      // The parser never yields an empty group, so "" only ever means "none".
      const key = entry.group ?? '';
      let bucket = buckets.get(key);

      if (!bucket) {
        bucket = { group: entry.group, entries: [] };
        buckets.set(key, bucket);
        grouped.push(bucket);
      }

      bucket.entries.push(entry);
    }

    return grouped;
  }, [entries]);

  const handleSelect = (entry: XmpProfileEntry) => {
    if (applyDisabled) return;

    // Re-selecting the active profile would reset its Amount and re-develop the
    // RAW for no visible change, so the active row is an explicit no-op.
    if (entry.path === activePath) return;

    onSelectProfile(entry);
  };

  return (
    <div className="mt-2">
      <button
        onClick={() => setIsExpanded((value) => !value)}
        aria-expanded={isExpanded}
        className="w-full flex items-center justify-center gap-1.5 py-1.5 rounded-md bg-bg-tertiary hover:bg-surface border border-surface text-sm text-text-primary transition-colors cursor-pointer"
        data-tooltip={t('ui.xmpProfile.browse')}
      >
        <FolderOpen size={14} />
        {t('ui.xmpProfile.browse')}
        <ChevronDown size={14} className={`transition-transform duration-200 ${isExpanded ? 'rotate-180' : ''}`} />
      </button>

      <AnimatePresence initial={false}>
        {isExpanded && (
          <motion.div
            initial={{ height: 0, opacity: 0 }}
            animate={{ height: 'auto', opacity: 1 }}
            exit={{ height: 0, opacity: 0 }}
            transition={{ duration: 0.25, ease: 'easeInOut' }}
            className="overflow-hidden"
          >
            <div className="pt-3">
              <div className="flex items-center justify-between mb-2">
                <span className="text-sm font-medium text-text-secondary select-none">
                  {t('ui.xmpProfile.profileLibrary')}
                </span>
                {root && !isLoading && (
                  <div className="flex items-center gap-2">
                    <button
                      onClick={() => discover(root)}
                      className="flex items-center text-text-secondary hover:text-accent transition-colors cursor-pointer"
                      data-tooltip={t('ui.xmpProfile.refresh')}
                    >
                      <RefreshCw size={12} />
                    </button>
                    <button
                      onClick={handleChooseFolder}
                      className="flex items-center text-text-secondary hover:text-accent transition-colors cursor-pointer"
                      data-tooltip={t('ui.xmpProfile.changeFolder')}
                    >
                      <FolderOpen size={12} />
                    </button>
                  </div>
                )}
              </div>

              {isLoading ? (
                <div className="flex items-center justify-center gap-2 p-3 text-text-secondary">
                  <Loader2 size={14} className="animate-spin shrink-0" />
                  <span className="text-sm select-none">{t('ui.xmpProfile.loading')}</span>
                </div>
              ) : !root ? (
                <button
                  onClick={handleChooseFolder}
                  className="w-full flex items-center justify-center gap-1.5 py-1.5 rounded-md bg-bg-tertiary hover:bg-surface border border-surface text-sm text-text-primary transition-colors cursor-pointer"
                >
                  <FolderOpen size={14} />
                  {t('ui.xmpProfile.chooseFolder')}
                </button>
              ) : entries.length === 0 ? (
                <p className="p-2 text-center text-xs text-text-secondary select-none">{t('ui.xmpProfile.empty')}</p>
              ) : (
                <div className="max-h-60 overflow-y-auto flex flex-col gap-2">
                  {groups.map((group) => (
                    <div key={group.group ?? ''}>
                      <span className="text-xs font-medium text-text-secondary select-none block mb-1">
                        {group.group ?? t('ui.xmpProfile.ungrouped')}
                      </span>
                      <div className="flex flex-col gap-0.5">
                        {group.entries.map((entry) => {
                          const isActive = entry.path === activePath;
                          const isClickable = !applyDisabled && !isActive;

                          return (
                            <button
                              key={entry.path}
                              onClick={() => handleSelect(entry)}
                              disabled={applyDisabled}
                              aria-current={isActive}
                              data-tooltip={applyDisabled ? t('ui.xmpProfile.rawOnly') : entry.name}
                              className={`flex items-center w-full text-left px-2 py-1.5 rounded-md text-sm border transition-colors disabled:cursor-not-allowed ${
                                isActive ? 'border-accent bg-accent/20' : 'border-transparent'
                              } ${applyDisabled && !isActive ? 'opacity-50' : ''} ${
                                isClickable ? 'hover:bg-card-active cursor-pointer' : ''
                              }`}
                            >
                              <span className="truncate min-w-0">{entry.name}</span>
                            </button>
                          );
                        })}
                      </div>
                    </div>
                  ))}
                </div>
              )}
            </div>
          </motion.div>
        )}
      </AnimatePresence>
    </div>
  );
}
