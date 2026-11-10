import {readFile,open} from 'node:fs/promises';
import {resolve} from 'node:path';
import {createRunnerClient} from '@lazybox/client-core';
import {environmentPaths} from './environments.mjs';
import {diagnosticRecorder} from './diagnostic-recording.ts';
const [sessionId,out,channel='dev']=process.argv.slice(2);
if(!sessionId||!out)throw Error('Usage: node --import tsx scripts/record-activity.mts SESSION OUTPUT.jsonl [dev|working]');
const endpoint=environmentPaths(resolve(process.env.LAZYBOX_V2_ENVIRONMENTS||'.cache/environments'),channel).endpoint;
const {url,token}=JSON.parse(await readFile(endpoint,'utf8'));
const api=createRunnerClient(url,token),file=await open(out,'wx',0o600),record=diagnosticRecorder();
const controller=new AbortController();process.once('SIGINT',()=>controller.abort());
const started=Date.now();let count=0;console.log('Recording sanitized activity. Ctrl-C stops recording; it does not stop the agent.');
try{for await(const row of api.watchEvents({sessionId,includePrevious:true},{signal:controller.signal})){if(row.event){const safe=record(row.event);if(safe){await file.write(JSON.stringify({...safe,elapsedMs:Date.now()-started})+'\n');count++;}}}}
catch(error){if(!controller.signal.aborted)throw error;}finally{await file.close();console.log(`Recorded ${count} activity events.`);}
