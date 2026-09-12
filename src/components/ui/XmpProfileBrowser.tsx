import { invoke } from '@tauri-apps/api/core';
import { open } from '@tauri-apps/plugin-dialog';
import { useCallback, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from 'react-toastify';
import { AnimatePresence, motion } from 'framer-motion';
import { ChevronDown, FolderOpen, Loader2, Plus, RefreshCw, X } from 'lucide-react';
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

/**
 * One folder of the session library, together with the profiles found in it.
 *
 * Keeping the scan result per root is what lets one folder be refreshed or
 * removed without disturbing the others.
 */
interface XmpProfileRoot {
  path: string;
  entries: XmpProfileEntry[];
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

/** Display-only label for a root: its last path segment. */
function rootLabel(path: string): string {
  const segments = path.split(/[\\/]/).filter(Boolean);
  return segments.length > 0 ? segments[segments.length - 1] : path;
}

/**
 * Ordering of a combined library, equivalent to the backend's own 4A ordering:
 * grouped profiles first, then group, name and path, each compared
 * case-insensitively except for the path tiebreaker.
 */
function compareEntries(a: XmpProfileEntry, b: XmpProfileEntry): number {
  if ((a.group === null) !== (b.group === null)) return a.group === null ? 1 : -1;

  const groupA = (a.group ?? '').toLowerCase();
  const groupB = (b.group ?? '').toLowerCase();
  if (groupA !== groupB) return groupA < groupB ? -1 : 1;

  const nameA = a.name.toLowerCase();
  const nameB = b.name.toLowerCase();
  if (nameA !== nameB) return nameA < nameB ? -1 : 1;

  if (a.path === b.path) return 0;
  return a.path < b.path ? -1 : 1;
}

/**
 * Session-only profile library browser over any number of folders.
 *
 * Applying a profile stays entirely with the parent, so the browser never
 * touches the profile transaction, the Amount control or image readiness: it
 * only hands a chosen entry back. Folders, their scan results and their loading
 * state live in component state and are never persisted or restored.
 */
export default function XmpProfileBrowser({ activePath, applyDisabled, onSelectProfile }: XmpProfileBrowserProps) {
  const { t } = useTranslation();

  const [isExpanded, setIsExpanded] = useState(false);
  const [roots, setRoots] = useState<XmpProfileRoot[]>([]);
  // Root paths with a scan in flight, so the same folder cannot be added or
  // refreshed twice while its first scan is still running.
  const [pendingPaths, setPendingPaths] = useState<ReadonlySet<string>>(() => new Set<string>());
  const [isRefreshingAll, setIsRefreshingAll] = useState(false);

  const setPending = useCallback((path: string, pending: boolean) => {
    setPendingPaths((previous) => {
      const next = new Set(previous);
      if (pending) next.add(path);
      else next.delete(path);
      return next;
    });
  }, []);

  const scan = useCallback(
    (path: string) => invoke<XmpProfileEntry[]>(Invokes.DiscoverXmpProfiles, { roots: [path] }),
    [],
  );

  const handleAddFolder = async () => {
    try {
      const selection = await open({ directory: true, multiple: false });

      // Cancelling the dialog must not scan, error or change any state.
      if (typeof selection !== 'string') return;

      // Root identity is the exact picked path, so re-adding the same folder is
      // a no-op that must not trigger a rescan.
      if (roots.some((root) => root.path === selection) || pendingPaths.has(selection)) return;

      setPending(selection, true);
      try {
        const entries = await scan(selection);
        // Only a successful scan adds the root. An empty folder is still a valid
        // root, it simply contributes no profiles.
        setRoots((previous) =>
          previous.some((root) => root.path === selection) ? previous : [...previous, { path: selection, entries }],
        );
      } catch (err) {
        // A folder that cannot be scanned is never added, so the roots that are
        // already loaded stay exactly as they were.
        console.error('Failed to discover XMP profiles:', err);
        toast.error(`${t('ui.xmpProfile.browseFailed')}: ${err}`);
      } finally {
        setPending(selection, false);
      }
    } catch (err) {
      console.error('Failed to choose profile folder:', err);
      toast.error(`${t('ui.xmpProfile.browseFailed')}: ${err}`);
    }
  };

  const handleRefreshRoot = async (path: string) => {
    if (pendingPaths.has(path)) return;

    setPending(path, true);
    try {
      const entries = await scan(path);
      // Replace only this root, and only while it is still part of the library:
      // a root removed mid-scan must not come back.
      setRoots((previous) => previous.map((root) => (root.path === path ? { ...root, entries } : root)));
    } catch (err) {
      // A failed refresh keeps this root's previous entries and keeps the root.
      console.error('Failed to refresh XMP profiles:', err);
      toast.error(`${t('ui.xmpProfile.browseFailed')}: ${err}`);
    } finally {
      setPending(path, false);
    }
  };

  const handleRefreshAll = async () => {
    if (isRefreshingAll || roots.length === 0) return;

    setIsRefreshingAll(true);
    try {
      // Sequential rather than parallel: one bounded call per root keeps each
      // root's entries owned by that root, which a single combined scan of 4A
      // cannot express.
      const refreshed: XmpProfileRoot[] = [];
      for (const root of roots) {
        refreshed.push({ path: root.path, entries: await scan(root.path) });
      }

      // Committed only once every root has scanned, so a failure anywhere leaves
      // the whole previous library in place. Roots added or removed during the
      // sweep keep whatever state they have by then.
      setRoots((previous) =>
        previous.map((root) => refreshed.find((scanned) => scanned.path === root.path) ?? root),
      );
    } catch (err) {
      console.error('Failed to refresh XMP profile folders:', err);
      toast.error(`${t('ui.xmpProfile.browseFailed')}: ${err}`);
    } finally {
      setIsRefreshingAll(false);
    }
  };

  const handleRemoveRoot = (path: string) => {
    // Session-only: this drops discovered metadata and nothing on disk, and it
    // deliberately leaves the active profile alone.
    setRoots((previous) => previous.filter((root) => root.path !== path));
  };

  const combinedEntries = useMemo(() => {
    const seen = new Set<string>();
    const merged: XmpProfileEntry[] = [];

    for (const root of roots) {
      for (const entry of root.entries) {
        // Path is the profile's identity, so only an exact path is a duplicate.
        // Overlapping roots therefore contribute one row per distinct file.
        if (seen.has(entry.path)) continue;
        seen.add(entry.path);
        merged.push(entry);
      }
    }

    // Each backend call sorted only its own root, so the combined list is
    // ordered here to match what a single 4A scan would have returned.
    return merged.sort(compareEntries);
  }, [roots]);

  // One first-appearance pass is enough once the combined list is ordered: the
  // ungrouped bucket can only be last, and profiles from different roots that
  // share a group merge under the same heading.
  const groups = useMemo(() => {
    const grouped: ProfileGroup[] = [];
    const buckets = new Map<string, ProfileGroup>();

    for (const entry of combinedEntries) {
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
  }, [combinedEntries]);

  const handleSelect = (entry: XmpProfileEntry) => {
    if (applyDisabled) return;

    // Re-selecting the active profile would reset its Amount and re-develop the
    // RAW for no visible change, so the active row is an explicit no-op.
    if (entry.path === activePath) return;

    onSelectProfile(entry);
  };

  const hasRoots = roots.length > 0;
  const isFirstScan = !hasRoots && pendingPaths.size > 0;

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
                  {t('ui.xmpProfile.profileFolders')}
                </span>
                {hasRoots && (
                  <button
                    onClick={handleRefreshAll}
                    disabled={isRefreshingAll || pendingPaths.size > 0}
                    className="flex items-center text-text-secondary hover:text-accent transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
                    data-tooltip={t('ui.xmpProfile.refreshAll')}
                  >
                    <RefreshCw size={12} className={isRefreshingAll ? 'animate-spin' : ''} />
                  </button>
                )}
              </div>

              {!hasRoots && (
                <p className="p-2 text-center text-xs text-text-secondary select-none">
                  {t('ui.xmpProfile.noFolders')}
                </p>
              )}

              {roots.map((root) => (
                <div key={root.path} className="flex items-center gap-2 px-2 py-1 rounded-md hover:bg-card-active">
                  <span className="truncate min-w-0 text-xs text-text-primary" data-tooltip={root.path}>
                    {rootLabel(root.path)}
                  </span>
                  <div className="ml-auto flex items-center gap-2 shrink-0">
                    {pendingPaths.has(root.path) ? (
                      <Loader2 size={12} className="animate-spin text-text-secondary" />
                    ) : (
                      <button
                        onClick={() => handleRefreshRoot(root.path)}
                        disabled={isRefreshingAll}
                        className="flex items-center text-text-secondary hover:text-accent transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
                        data-tooltip={t('ui.xmpProfile.refreshFolder')}
                      >
                        <RefreshCw size={12} />
                      </button>
                    )}
                    <button
                      onClick={() => handleRemoveRoot(root.path)}
                      disabled={isRefreshingAll}
                      className="flex items-center text-text-secondary hover:text-accent transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
                      data-tooltip={t('ui.xmpProfile.removeFolder')}
                    >
                      <X size={12} />
                    </button>
                  </div>
                </div>
              ))}

              <button
                onClick={handleAddFolder}
                disabled={isRefreshingAll || isFirstScan}
                className={`w-full flex items-center justify-center gap-1.5 py-1.5 rounded-md bg-bg-tertiary hover:bg-surface border border-surface text-sm text-text-primary transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed ${
                  hasRoots ? 'mt-1' : ''
                }`}
                data-tooltip={t('ui.xmpProfile.addFolder')}
              >
                {isFirstScan ? (
                  <>
                    <Loader2 size={14} className="animate-spin shrink-0" />
                    {t('ui.xmpProfile.loading')}
                  </>
                ) : (
                  <>
                    <Plus size={14} />
                    {t('ui.xmpProfile.addFolder')}
                  </>
                )}
              </button>

              {hasRoots && (
                <>
                  <div className="my-2 border-t border-border-color/50" />

                  {combinedEntries.length === 0 ? (
                    <p className="p-2 text-center text-xs text-text-secondary select-none">
                      {t('ui.xmpProfile.empty')}
                    </p>
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
                </>
              )}
            </div>
          </motion.div>
        )}
      </AnimatePresence>
    </div>
  );
}
