
export function repositoryOrganization(identity:string){return identity.startsWith('github:')?identity.slice(7).split('/')[0]:'Local repositories';}
export function compareRepositories(a:{identity:string;name:string},b:{identity:string;name:string}){
 const left=repositoryOrganization(a.identity),right=repositoryOrganization(b.identity);
 if(left==='Local repositories'&&right!==left)return 1;if(right==='Local repositories'&&left!==right)return -1;
 return left.localeCompare(right,undefined,{sensitivity:'base'})||a.name.localeCompare(b.name,undefined,{sensitivity:'base'});
}

export function repositoryLabel(repo:{gitIdentity:string;name:string;id?:string}){return repo.gitIdentity.startsWith('github:')?repo.gitIdentity.slice(7):repo.name||repo.id||'Local repository';}

/**
 * The branch a workspace name suggests: "Login fix" → "feature/login-fix".
 * Only characters every Git ref accepts; '' when the name has none.
 */
export function branchForName(name:string):string{
 const slug=name.normalize('NFKD').replace(/[̀-ͯ]/g,'').toLowerCase().replace(/[^a-z0-9._-]+/g,'-').replace(/\.{2,}/g,'.').replace(/^[-.]+|[-.]+$/g,'').replace(/-{2,}/g,'-').slice(0,60).replace(/[-.]+$/,'').replace(/\.lock$/,'');
 return slug?`feature/${slug}`:'';
}

/**
 * True for a name the Runner generated ("gentle-copper-otter") rather than one
 * a person chose: three lowercase words joined by hyphens.
 */
export function generatedName(name:string):boolean{return /^[a-z]+-[a-z]+-[a-z]+$/.test(name.trim());}

/** Splits "owner/name" labels: the owner once when every repository shares it. */
export function repositoryGroups(labels:readonly string[]):{owner:string;repositories:{full:string;short:string}[]}{
 const parts=labels.map(full=>{const slash=full.indexOf('/');return {full,owner:slash>0?full.slice(0,slash):'',short:slash>0?full.slice(slash+1):full};});
 const owner=parts.length>0&&parts[0].owner&&parts.every(p=>p.owner===parts[0].owner)?parts[0].owner:'';
 return {owner,repositories:parts.map(p=>({full:p.full,short:owner?p.short:p.full}))};
}

/**
 * A readable default name for a new workspace from its repositories' names
 * ("owner/name" or "name"): one repository is its name, two short ones are
 * "a + b", more are the first and how many others ("authority +4"), never a
 * run-on list of every name.
 */
export function suggestWorkspaceName(labels:readonly string[]):string{
 const names=labels.map(label=>label.split('/').at(-1)||label).filter(Boolean);
 if(!names.length)return 'New workspace';
 if(names.length===1)return names[0].slice(0,128);
 if(names.length===2&&names[0].length+names[1].length<=40)return `${names[0]} + ${names[1]}`;
 return `${names[0].slice(0,100)} +${names.length-1}`;
}
