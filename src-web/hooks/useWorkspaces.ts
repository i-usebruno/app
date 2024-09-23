import type { Workspace } from '@yaakapp-internal/models';
import { atom, useAtomValue } from 'jotai';
import { listWorkspaces } from '../lib/store';

const workspaces = await listWorkspaces();
export const workspacesAtom = atom<Workspace[]>(workspaces);

export function useWorkspaces() {
  return useAtomValue(workspacesAtom);
}
