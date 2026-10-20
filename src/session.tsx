// Session context shared by the shell and the pages: current view, the
// inspector, live tasks, recent matters and the default generation connection.
import { createContext, useContext } from 'react';
import type { Connection, DueTodo, EvidenceItem, Locator, Matter, TaskState, UnlockInfo } from './api';
import type { QaTurn } from './qa';

export type View =
  | { name: 'home' }
  | { name: 'matters' }
  | { name: 'matter'; id: string; tab?: string }
  | { name: 'qa'; matters?: string[]; question?: string }
  | { name: 'connections' }
  | { name: 'knowledge' }
  | { name: 'tools' }
  | { name: 'settings'; tab?: string };

/** A source location to show in the inspector. */
export type ViewerTarget = { version: string; title: string; page?: string; highlight?: Locator[]; media?: boolean };

/** Evidence of one answer or search; `labels` maps evidence id to its citation number. */
export type EvidencePanel = { title: string; note?: string; items: EvidenceItem[]; labels?: Record<string, string>; current?: string | null };

export type InspectorTarget = { kind: 'source'; source: ViewerTarget } | { kind: 'evidence'; evidence: EvidencePanel; source?: ViewerTarget };

export type Session = {
  info: UnlockInfo;
  view: View;
  go: (view: View) => void;
  /** Open a source location in the inspector. */
  open: (target: ViewerTarget) => void;
  inspect: (target: InspectorTarget | null) => void;
  inspector: InspectorTarget | null;
  tasks: TaskState[];
  revision: number;
  refresh: () => void;
  matters: Matter[];
  /** False until the matter list has been read once. */
  mattersLoaded: boolean;
  /** Open todos of all matters, overdue or due within two weeks. */
  due: DueTodo[];
  connections: Connection[];
  /** The default active connection, if any. */
  connection: Connection | null;
  reloadConnections: () => void;
  newMatter: () => void;
  lock: () => void;
  qa: { turns: QaTurn[]; update: (change: (turns: QaTurn[]) => QaTurn[]) => void };
};

export const SessionContext = createContext<Session | null>(null);

export function useSession(): Session {
  const session = useContext(SessionContext);
  if (!session) throw new Error('session missing');
  return session;
}

/** Inspector target for one evidence item (page and highlighted boxes). */
export function sourceOf(item: EvidenceItem): ViewerTarget {
  return {
    version: item.version_id,
    title: item.document.display_name,
    page: item.pages[0],
    highlight: item.highlight_indexes.map((i) => item.locators[i]).filter(Boolean),
    media: item.document.media_kind !== 'document',
  };
}
