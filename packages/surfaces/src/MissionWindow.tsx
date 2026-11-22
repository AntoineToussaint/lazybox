import type { ViewStateStore } from '@lazybox/platform';
import { AppearanceSync } from './appearance/AppearanceSync';
import { MissionsPanel } from './MissionsPanel';
import { RunnerProvider, useRunner, type RunnerConnection } from './RunnerContext';
import { useWorkspaceCatalog } from './useWorkspaceCatalog';
import { requestNavigation } from './navigate-requests';
import { useConversationRequests } from './review-client';
import type { ViewTarget } from './workspace-model';

type Props = { missionId: string; viewState: ViewStateStore; onFocusMain(): void };
export function MissionWindow({connection, ...props}: Props & {connection: RunnerConnection}) {
  return <RunnerProvider connection={connection}><AppearanceSync store={props.viewState}/><MissionView {...props}/></RunnerProvider>;
}

function MissionView({missionId, viewState, onFocusMain}: Props) {
  const {api} = useRunner();
  const {collections} = useWorkspaceCatalog(api);
  const show = (target: ViewTarget) => {
    requestNavigation(viewState.root || viewState.key, target);
    onFocusMain();
  };
  useConversationRequests('', request => show({...request, commandId: ''}));
  return <main className="workspace-shell"><MissionsPanel workspaceId="" collections={collections} pinned={missionId} viewState={viewState}
    onShowSession={show} onShowPanel={(_, target) => { if (target) show(target); }}/></main>;
}
