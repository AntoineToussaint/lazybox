/** Diagnostic recordings use an allowlist. Prompts, tool inputs/results, auth,
 * paths, native IDs and reasoning text never cross this boundary. */
export function diagnosticRecorder(){
 const actors=new Map<string,string>(),tools=new Map<string,string>();let cursor=0;
 const alias=(map:Map<string,string>,key:string,prefix:string)=>{if(!map.has(key))map.set(key,`${prefix}-${map.size+1}`);return map.get(key)!;};
 const knownNames=new Set(['Read','Write','Edit','Bash','Grep','Glob','Agent','Task','Shell command','command']);
 const name=(n:unknown)=>typeof n==='string'?(knownNames.has(n)?n:alias(tools,n,'tool')):'Tool';
 return (e:any)=>{
  if(!['session_ready','turn_started','turn_ended','activity','tool_activity','request_resolved','failure','session_closed'].includes(e.type))return;
  const event:any={type:e.type};
  if(e.status&&['compacting','requesting','working','thinking','responding','tool','running','completed','failed','interrupted'].includes(e.status))event.status=e.status;
  if(e.agentId&&e.agentId!=='root')event.agentId=alias(actors,e.agentId,'agent');
  if(e.name)event.name=name(e.name);
  if(e.type==='tool_activity'){
   let d:any;try{d=JSON.parse(new TextDecoder().decode(e.providerDataJson));}catch{return;}
   let safe:any;
   if(d.type==='tool_call')safe={type:'tool_call',id:alias(tools,String(d.id),'call'),name:d.name?name(d.name):undefined,status:['running','completed','failed'].includes(d.status)?d.status:undefined,input:d.input===undefined?undefined:'[omitted]',output:d.output===undefined?undefined:'[omitted]'};
   else if(d.type==='tool_use')safe={type:'tool_use',id:alias(tools,String(d.id),'call'),name:name(d.name),input:'[omitted]'};
   else if(d.type==='tool_result')safe={type:'tool_result',tool_use_id:alias(tools,String(d.tool_use_id),'call'),is_error:!!d.is_error,content:'[omitted]'};
   else if(['commandExecution','fileChange','mcpToolCall','dynamicToolCall','webSearch','imageView','collabAgentToolCall'].includes(d.item?.type))safe={item:{id:alias(tools,String(d.item.id),'call'),type:d.item.type,tool:name(d.item.tool),status:['completed','failed','declined','inProgress'].includes(d.item.status)?d.item.status:undefined,exitCode:typeof d.item.exitCode==='number'?d.item.exitCode:undefined}};
   else return;
   event.providerDataJson=Buffer.from(JSON.stringify(safe)).toString('base64');
  }
  return {cursor:String(++cursor),event};
 };
}
