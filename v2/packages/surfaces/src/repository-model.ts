
export function repositoryOrganization(identity:string){return identity.startsWith('github:')?identity.slice(7).split('/')[0]:'Local repositories';}
export function compareRepositories(a:{identity:string;name:string},b:{identity:string;name:string}){
 const left=repositoryOrganization(a.identity),right=repositoryOrganization(b.identity);
 if(left==='Local repositories'&&right!==left)return 1;if(right==='Local repositories'&&left!==right)return -1;
 return left.localeCompare(right,undefined,{sensitivity:'base'})||a.name.localeCompare(b.name,undefined,{sensitivity:'base'});
}

export function repositoryLabel(repo:{gitIdentity:string;name:string;id?:string}){return repo.gitIdentity.startsWith('github:')?repo.gitIdentity.slice(7):repo.name||repo.id||'Local repository';}
