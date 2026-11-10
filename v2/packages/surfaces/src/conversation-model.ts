import type { Session, Profile } from '@lazybox/protocol';

/** Creation order is independent of selection, status, and RPC response order.
 * Older Runners omit timestamps; stable IDs provide a deterministic tie-break. */
export function mergeConversations(saved: readonly Session[], live: readonly Session[]): Session[] {
  const rows = new Map(saved.map(session => [session.id, session]));
  for (const session of live) {
    const previous = rows.get(session.id);
    rows.set(session.id, {
      ...previous, ...session,
      title: session.title || previous?.title || '',
      createdAtUnixMillis: session.createdAtUnixMillis || previous?.createdAtUnixMillis || 0n,
    });
  }
  const superseded = new Set([...rows.values()].map(session => session.previousExecutionId).filter(Boolean));
  return [...rows.values()].filter(session => !superseded.has(session.id)).sort((a, b) => {
    if (a.createdAtUnixMillis !== b.createdAtUnixMillis) return a.createdAtUnixMillis > b.createdAtUnixMillis ? -1 : 1;
    const left=a.conversationId || a.id, right=b.conversationId || b.id;
    return left < right ? -1 : left > right ? 1 : 0;
  });
}

/** Never present an account profile ID as model information. */
export function sessionModelLabel(session?:Session,reportedModel=''):string {
 if(reportedModel)return reportedModel;
 if(session?.model)return session.modelReported?session.model:`${session.model} (requested)`;
 return 'Model not reported yet';
}
export function sessionAccountLabel(session?:Session,profile?:Profile):string {
 if(profile?.label&&profile.id!=='claude-default'&&profile.id!=='codex-default')return profile.label;
 if(profile?.isolation==='inherited'||session?.profileId==='claude-default'||session?.profileId==='codex-default')return 'Account not reported';
 return session?.profileId?'Saved account':'Loading account…';
}
export function conversationDetails(session:Session,profiles:readonly Profile[],includeAccount=true):string {
 const profile=profiles.find(value=>value.id===session.profileId);
 const provider=session.provider||profile?.provider||'';
 const agent=provider==='claude'?'Claude':provider==='codex'?'Codex':provider||'Agent';
 return `${agent} · ${sessionModelLabel(session)}${includeAccount?` · ${sessionAccountLabel(session,profile)}`:''}`;
}
