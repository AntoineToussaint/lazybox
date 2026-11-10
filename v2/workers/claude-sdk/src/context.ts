import type { SDKControlGetContextUsageResponse } from '@anthropic-ai/claude-agent-sdk';

/** Keep names/counts, never schemas, prompt contents or absolute memory paths. */
export function contextSnapshot(report: SDKControlGetContextUsageResponse) {
 const valid=(n:number)=>Number.isSafeInteger(n)&&n>=0;
 type Detail={name:string;tokens:number};
 const details:Record<string,Detail[]>={};
 const add=(key:string,name:string,tokens:number)=>{if(name&&valid(tokens)&&(details[key]?.length||0)<256)(details[key]??=[]).push({name:name.slice(0,160),tokens});};
 for(const row of report.systemTools||[])add('system_tools',row.name,row.tokens);
 for(const row of report.systemPromptSections||[])add('system_prompt',row.name,row.tokens);
 for(const row of report.mcpTools||[])add(row.isLoaded===false?'mcp_tools_deferred':'mcp_tools',`${row.serverName} / ${row.name}`,row.tokens);
 for(const row of report.mcpInstructions||[])add('mcp_instructions',row.serverName,row.tokens);
 for(const row of report.deferredBuiltinTools||[])if(!row.isLoaded)add('system_tools_deferred',row.name,row.tokens);
 for(const row of report.memoryFiles||[])add('memory_files',`${row.type || 'Memory'} · ${row.path?.split(/[\\/]/).pop()||'file'}`,row.tokens);
 for(const row of report.skills?.skillFrontmatter||[])add('skills',row.name,row.tokens);
 for(const row of report.agents||[])add('custom_agents',row.agentType,row.tokens);
 const b=report.messageBreakdown;
 if(b)for(const [key,name] of Object.entries({toolCallTokens:'Tool calls',toolResultTokens:'Tool results',attachmentTokens:'Attachments',assistantMessageTokens:'Assistant messages',userMessageTokens:'Your messages',redirectedContextTokens:'Redirected context',unattributedTokens:'Other messages'}))add('messages',name,b[key as keyof typeof b] as number);
 const categories=report.categories.filter(row=>valid(row.tokens)).slice(0,64).map(row=>({name:row.id||row.name.slice(0,80),tokens:row.tokens,kind:row.kind,included:row.kind==='used',details:details[row.id||'']||[]}));
 return {model:report.model,used:valid(report.totalTokens)?report.totalTokens:undefined,
  capacity:valid(report.rawMaxTokens)?report.rawMaxTokens:valid(report.maxTokens)?report.maxTokens:undefined,
  reserve:categories.filter(row=>row.kind==='buffer').reduce((n,row)=>n+row.tokens,0),categories,source:'sdk_summary_estimate'};
}
