import { useEffect, useState } from 'react';
import type { ModelPrice, RunnerSettings } from '@lazybox/protocol';
import { useRunner } from './RunnerContext';

export function useRunnerSettings(){
 const {api}=useRunner();const [settings,setSettings]=useState<RunnerSettings>(),[error,setError]=useState(''),[loading,setLoading]=useState(true);
 useEffect(()=>{const control=new AbortController();const load=()=>{void api.getRunnerSettings({}, {signal:control.signal,timeoutMs:5000}).then(value=>{if(!control.signal.aborted){setSettings(value);setError('');}},e=>{if(!control.signal.aborted)setError(String(e));}).finally(()=>{if(!control.signal.aborted)setLoading(false);});};load();window.addEventListener('lazybox-settings-saved',load);return()=>{control.abort();window.removeEventListener('lazybox-settings-saved',load);};},[api]);
 return {settings,setSettings,error,setError,loading};
}
const announce=()=>window.dispatchEvent(new Event('lazybox-settings-saved'));
export function AgentSettings(){
 const {api}=useRunner();const {settings,setSettings,error,setError,loading}=useRunnerSettings();const [busy,setBusy]=useState(false),[saved,setSaved]=useState(false);
 if(!settings)return <p role={error?'alert':'status'}>{error||'Loading agent settings…'}</p>;
 return <form className="runner-preferences" onSubmit={async event=>{event.preventDefault();setBusy(true);setSaved(false);try{setSettings(await api.saveRunnerSettings(settings));announce();setSaved(true);}catch(e){setError(String(e));}finally{setBusy(false);}}}>
  <h2>Agents & workspaces</h2><p>Defaults apply to new sessions. Existing sessions and resumed conversations keep their saved settings.</p>
  <label>Suggested workspace names<select value={settings.naming} onChange={e=>{setSettings({...settings,naming:e.target.value});setSaved(false);}}><option value="repositories">Repository names</option><option value="generated">Memorable generated names</option></select></label>
  <small>Suggestions follow repository selection until you type a custom name. The disk folder gets its own fixed slug.</small>
  <label>Default permissions<select value={settings.permission} onChange={e=>{setSettings({...settings,permission:e.target.value});setSaved(false);}}><option value="">Current CLI settings</option><option value="ask">Ask for approval</option><option value="read-only">Read only</option><option value="yolo">YOLO · full access, no approval prompts</option></select></label>
  {settings.permission==='yolo'&&<p>New coding sessions will run with unrestricted tool access. Individual sessions can override this choice.</p>}
  <label>Lazybox context for every new agent<textarea rows={7} maxLength={32768} value={settings.context} onChange={e=>setSettings({...settings,context:e.target.value})}/></label>
  <label>Additional Claude context<textarea rows={3} maxLength={16384} value={settings.claudeContext} onChange={e=>setSettings({...settings,claudeContext:e.target.value})}/></label>
  <label>Additional Codex context<textarea rows={3} maxLength={16384} value={settings.codexContext} onChange={e=>setSettings({...settings,codexContext:e.target.value})}/></label>
  <small>Context is appended to the provider's instructions alongside workspace facts. It does not grant permissions or replace repository instructions.</small>
  {error&&<p role="alert">{error} <button type="button" onClick={announce}>Reload settings</button></p>}{saved&&<p role="status">Agent settings saved.</p>}<button disabled={busy||loading} className="primary">{busy?'Saving…':'Save agent settings'}</button>
 </form>;
}
const priceFields=['input','output','cacheRead','cacheWrite5m','cacheWrite1h','maxInput'] as const;
const labels=['Input','Output','Cache read','Cache write · 5 minutes','Cache write · 1 hour','Maximum input tokens (0 = no threshold)'];
export function PricingSettings(){
 const {api}=useRunner();const {settings,setSettings,error,setError}=useRunnerSettings();const [provider,setProvider]=useState('claude'),[model,setModel]=useState(''),[values,setValues]=useState(['','','','','','0']),[busy,setBusy]=useState(false),[saved,setSaved]=useState('');
 const select=(p:ModelPrice)=>{setProvider(p.provider);setModel(p.model);setValues(priceFields.map(k=>String(Number(p[k])/(k==='maxInput'?1:1000))));setSaved('');setError('');};
 if(!settings)return <p role={error?'alert':'status'}>{error||'Loading model prices…'}</p>;
 const current=settings.prices.find(p=>p.provider===provider&&p.model===model);
 const apply=async(remove=false)=>{setBusy(true);setError('');setSaved('');try{
  const parsed=values.map((text,i)=>{const value=Number(text)*(i===5?1:1000);if(!remove&&(text.trim()===''||!Number.isFinite(value)||!Number.isSafeInteger(Math.round(value))||Math.abs(value-Math.round(value))>1e-6||value<0))throw Error('Enter nonnegative prices with at most three decimal places and a whole-token threshold.');return BigInt(Math.round(value)||0);});
  setSettings(await api.setModelPrice({revision:settings.revision,remove,price:{provider,model:model.trim(),input:parsed[0],output:parsed[1],cacheRead:parsed[2],cacheWrite5m:parsed[3],cacheWrite1h:parsed[4],maxInput:parsed[5]}}));announce();setSaved(remove?'Custom override removed.':'Model price saved. Retained unpriced usage will use this rate when enough usage detail is available.');
 }catch(e){setError(String(e));}finally{setBusy(false);}};
 return <section className="runner-preferences"><h2>Model pricing</h2><p>USD per million tokens. Exact model IDs; standard service tier. Estimates are separate from subscription limits and invoices.</p>
 <div className="price-catalog">{settings.prices.map(p=><button key={`${p.provider}/${p.model}`} onClick={()=>select(p)}><strong>{p.model}</strong><small>{p.provider} · ${Number(p.input)/1000} input / ${Number(p.output)/1000} output · {p.custom?'Custom':p.version}</small></button>)}</div>
 <form onSubmit={e=>{e.preventDefault();void apply();}}><div className="setup-row"><label>Provider<select value={provider} onChange={e=>setProvider(e.target.value)}><option value="claude">Claude</option><option value="codex">Codex</option></select></label><label>Exact model ID<input required maxLength={128} placeholder="claude-fable-5-1" value={model} onChange={e=>setModel(e.target.value)}/></label></div>
 <div className="price-fields">{priceFields.map((field,i)=><label key={field}>{labels[i]}<input type="number" required min={i<2?'0.001':'0'} step={i===5?'1':'0.001'} value={values[i]} onChange={e=>setValues(values.map((v,j)=>j===i?e.target.value:v))}/></label>)}</div>
 {error&&<p role="alert">{error} <button type="button" onClick={announce}>Reload settings</button></p>}{saved&&<p role="status">{saved}</p>}<button className="primary" disabled={busy}>Save model price</button>{current?.custom&&<button type="button" disabled={busy} onClick={()=>void apply(true)}>Remove custom override</button>}
 </form><small>Already-priced records retain their captured rate version. Missing prices are estimated using the current rate. Incomplete usage and unsupported service tiers remain unpriced.</small></section>;
}
