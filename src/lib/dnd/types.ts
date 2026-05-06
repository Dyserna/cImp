// Shared types for the drag-and-drop layer. Kept separate from the
// state-machine module so the layout store can import `DropTarget`
// without pulling in the live `dragState` writable (which would create
// a cycle with the commit code).

import type { PaneId } from '../layout/types';
import type { TabId } from '../tabs/types';

export type DropTarget =
  | { kind: 'reorder'; paneId: PaneId; insertIndex: number }
  | { kind: 'moveToPane'; paneId: PaneId }
  | { kind: 'split'; paneId: PaneId; direction: 'left' | 'right' | 'top' | 'bottom' };

export type DragState =
  | { kind: 'idle' }
  | {
      kind: 'pending';
      tabId: TabId;
      sourcePaneId: PaneId;
      startX: number;
      startY: number;
      pointerId: number;
      sourceEl: Element;
    }
  | {
      kind: 'dragging';
      tabId: TabId;
      sourcePaneId: PaneId;
      cursorX: number;
      cursorY: number;
      pointerId: number;
      sourceEl: Element;
      dropTarget: DropTarget | null;
    };
