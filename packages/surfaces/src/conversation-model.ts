import type { Collection, Session, Profile } from '@lazybox/protocol';
import { agentTitle, isAcp } from './providers';

/**
 * The conversation's own title, or '' while it has none. The Runner starts a
 * conversation titled after its working folder, which for a workspace is the
 * workspace's generated name ("strongly-tight-collie"): that is not a title.
 */
export function conversationTitle(session?:Session):string{
 const title=session?.title?.trim()||'';
 if(!title)return '';
 const folder=(session?.workingDirectory||'').split('/').filter(Boolean).at(-1)||'';
 return title===folder?'':title;
}
/**
 * What every surface calls a conversation (the header, tabs, sidebar rows, the
 * Agents panel, inbox, peek, palette and the phone list): its title — the
 * person's name for it, the title generated after its first turn, or one made
 * from its first message, as the Runner resolves it — else `fallback`.
 */
export function conversationName(session?:Session,fallback='New conversation'):string{
 return conversationTitle(session)||fallback;
}
/**
 * A conversation in a list of a workspace's conversations: "Title · Claude",
 * with its subscription's name when it has one ("· Work Claude"). Two that
 * would read the same are told apart by the start of their ids, and only then:
 * an id is not something to read.
 */
export function conversationOption(session:Session,all:readonly Session[],provider:(profileId:string,session:Session)=>string,label:(profileId:string)=>string|undefined):string{
 const words=(value:Session)=>[conversationName(value),provider(value.profileId,value),label(value.profileId)].filter(Boolean).join(' · ');
 const text=words(session);
 return all.some(other=>other.id!==session.id&&words(other)===text)?`${text} · ${session.id.slice(0,6)}`:text;
}
// Words that only open a request ("I want to", "can you"); the Runner's titles.Fallback drops the same.
const FILLER=/^(?:(?:hi|hey|hello|ok|okay|so|alright|please|pls)\b[\s,.!:;-]*|(?:i|we)\s+(?:want|wanna|need|have)\s+(?:you\s+)?to\b\s*|(?:i|we)\s*(?:'d|’d|\s+would)\s+like\s+(?:you\s+)?to\b\s*|(?:i|we)\s*(?:'m|’m|'re|’re|\s+am|\s+are)\s+(?:going|trying)\s+to\b\s*|(?:can|could|would|will)\s+(?:you|we)\s+(?:please\s+)?|let['’]?s\b\s*|help\s+me\s+(?:to\s+)?|i\s+think\s+(?:we\s+should\s+)?|we\s+should\s+)/i;
const DANGLING=new Set('a an the to of and or but for with in on at is are be we i it that this so as by from into about if then my our your'.split(' '));
/** Cuts text to at most `limit` characters at a word, drops a dangling small word, adds an ellipsis. */
export function cutAtWord(text:string,limit:number):string{
 if(text.length<=limit)return text;
 const head=text.slice(0,limit),space=head.lastIndexOf(' ');
 const words=(space>0?head.slice(0,space):text.slice(0,limit-1)).split(' ');
 while(words.length>1&&DANGLING.has(words.at(-1)!.toLowerCase().replace(/[,.;:!?—-]+$/,'')))words.pop();
 return words.join(' ').replace(/[\s,.;:!?—-]+$/,'')+'…';
}
/**
 * A short title suggested from a first message, as the Runner makes one: its
 * first sentence or line without markup or the words that only open a request,
 * capitalised, cut at a word with an ellipsis — never inside a word.
 */
export function titleFromMessage(text:string,limit=60):string{
 let line=text.replace(/```[\s\S]*?(```|$)/g,'\n').split('\n').map(value=>value.replace(/[#>*_`~]+/g,'').trim().replace(/^(?:[-+•]|\d+[.)])\s+/,'').replace(/\s+/g,' ').trim()).find(value=>/[\p{L}\p{N}]/u.test(value))||'';
 for(let i=0;i<4;i++){const next=line.replace(FILLER,'').replace(/^[\s,.:;-]+/,'');if(next===line||next.length<3)break;line=next;}
 line=line.charAt(0).toLocaleUpperCase()+line.slice(1);
 const sentence=/^(.{12,}?[.?!])(?:\s|$)/.exec(line)?.[1];
 if(sentence&&sentence.length<=limit)return sentence.replace(/[.!:;,]+$/,'');
 if(line.length<=limit)return line.replace(/[.!:;,]+$/,'');
 return cutAtWord(line,limit);
}
/**
 * One line for a workspace's branch: the branch once when every repository is
 * on it ("feature/x", "5 repositories"), otherwise the first and "+N".
 */
export function branchSummary(workspace?:Collection):{name:string;detail:string;title:string}|undefined{
 const members=workspace?.members.filter(member=>member.spec?.branch)||[];
 if(!members.length)return undefined;
 const branches=[...new Set(members.map(member=>member.spec!.branch))];
 const title=members.map(member=>`${member.spec!.name}: ${member.spec!.branch}`).join('\n');
 if(branches.length===1)return {name:branches[0],detail:members.length>1?`${members.length} repositories`:'',title};
 return {name:branches[0],detail:`+${branches.length-1} ${branches.length===2?'branch':'branches'}`,title};
}

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
/**
 * A model's name as people say it: "claude-opus-5-5" is "Opus 5.5",
 * "gpt-5.1-codex-max" is "GPT-5.1 Codex Max". An id it does not recognise
 * stays as it is; " (requested)" is said in the tooltip instead.
 */
export function modelName(id: string): string {
  const raw = id.replace(/\s*\(requested\)$/, '').trim();
  const claude = /^claude-([a-z]+)-(\d+)(?:-(\d{1,2}))?(?:-\d{8})?(\[1m\])?$/i.exec(raw);
  if (claude) return `${claude[1][0].toUpperCase()}${claude[1].slice(1)} ${claude[2]}${claude[3] ? '.' + claude[3] : ''}${claude[4] ? ' (1M)' : ''}`;
  const gpt = /^gpt-(\d+(?:\.\d+)?)((?:-[a-z]+)*)$/i.exec(raw);
  if (gpt) return `GPT-${gpt[1]}${gpt[2].split('-').filter(Boolean).map(word => ' ' + word[0].toUpperCase() + word.slice(1)).join('')}`;
  return raw;
}
/** "Claude · Opus 5.5", or the agent alone before a model is known: the chat header and the message box say the same. */
export function agentModelText(agentName: string, modelLabel: string): string {
  const model = modelName(modelLabel);
  return model ? `${agentName} · ${model}` : agentName;
}
export function sessionModelLabel(session?:Session,reportedModel=''):string {
 if(reportedModel)return reportedModel;
 if(session?.model)return session.modelReported?session.model:`${session.model} (requested)`;
 return '';
}
export function sessionAccountLabel(session?:Session,profile?:Profile):string {
 // An ACP agent signs in on its own: Lazybox has no account to name.
 if(isAcp(profile?.provider||session?.provider))return '';
 if(profile?.label&&profile.id!=='claude-default'&&profile.id!=='codex-default')return profile.label;
 if(profile?.isolation==='inherited'||session?.profileId==='claude-default'||session?.profileId==='codex-default')return 'Account not reported';
 return session?.profileId?'Saved account':'Loading account…';
}
export function conversationProvider(session: Session, profiles: readonly Profile[]): string {
  return session.provider || profiles.find(profile => profile.id === session.profileId)?.provider || '';
}

export function conversationDetails(session:Session,profiles:readonly Profile[],includeAccount=true):string {
 const profile=profiles.find(value=>value.id===session.profileId);
 const provider=conversationProvider(session,profiles);
 const agent=agentTitle(provider,profiles);
 return [agent,sessionModelLabel(session),includeAccount&&!isAcp(provider)?sessionAccountLabel(session,profile):''].filter(Boolean).join(' · ');
}
/**
 * The session a conversation window shows for the execution it is on. When the
 * window moves to another execution (a successor, a linked conversation), the
 * session it holds is the previous one until GetSession answers, and its
 * conversation would be read for the new one (usage, prompts, links). What the
 * new execution's reader already knows (a continuation carries its session)
 * stands in meanwhile; otherwise there is none yet.
 */
export function currentSession(sessionId:string,held?:Session,known?:Session):Session|undefined{
 return held?.id===sessionId?held:known?.id===sessionId?known:undefined;
}
