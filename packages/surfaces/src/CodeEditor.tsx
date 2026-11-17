import { useEffect, useRef, useState, type KeyboardEvent } from 'react';
import * as monaco from 'monaco-editor';
import './monaco-languages';
import { getAppearance, subscribeAppearance } from './appearance/apply';
import { codeFont, monacoTheme } from './appearance/integrations';
import EditorWorker from 'monaco-editor/editor/editor.worker.js?worker';
import JsonWorker from 'monaco-editor/language/json/json.worker.js?worker';

import TsWorker from 'monaco-editor/language/typescript/ts.worker.js?worker';
import CssWorker from 'monaco-editor/language/css/css.worker.js?worker';
import HtmlWorker from 'monaco-editor/language/html/html.worker.js?worker';

globalThis.MonacoEnvironment = { getWorker: (_id, label) => label === 'json' ? new JsonWorker() : ['typescript','javascript'].includes(label) ? new TsWorker() : ['css','scss','less'].includes(label) ? new CssWorker() : ['html','handlebars','razor'].includes(label) ? new HtmlWorker() : new EditorWorker() };
// The editor's theme and font come from Settings → Appearance and follow it live.
const editorFont=()=>{const f=codeFont(getAppearance()),size=Math.round(13*f.size/12);return {fontFamily:f.family,fontSize:size,lineHeight:Math.round(size*1.6),fontLigatures:f.ligatures};};
const paintEditors=()=>{monaco.editor.defineTheme('lazybox',monacoTheme(getAppearance()));monaco.editor.setTheme('lazybox');};
paintEditors();
subscribeAppearance(paintEditors);

/** Replace a model's text as one undoable edit, keeping the cursor where it can
 * be: a file reloaded from disk is not a new document. */
function replaceText(model:monaco.editor.ITextModel,value:string){
 model.pushStackElement();
 model.pushEditOperations([],[{range:model.getFullModelRange(),text:value}],()=>null);
 model.pushStackElement();
}

// monaco-vim's ex commands are global; each one finds its editor's handlers here.
type VimAdapter = { editor: monaco.editor.ICodeEditor; on(event: string, handler: (...args: never[]) => void): void; openNotification(text: string): void; dispose(): void };
type ExHandlers = { write(thenQuit: boolean): void; quit(force: boolean): void };
/**
 * STOPGAP (not a fix): Monaco's WebKit clipboard workaround
 * (BrowserClipboardService.installWebKitWriteTextWorkaround) hands
 * `navigator.clipboard.write` a deferred promise and cancels the previous one
 * on every click or key. When the editor has been in a popped-out window,
 * WebKit leaves those writes pending, so each cancel surfaces as an unhandled
 * "Canceled" rejection. Monaco treats its CancellationError as expected
 * everywhere else (isCancellationError); this does the same for the ones it
 * leaves unhandled, and only for those. Remove once Monaco handles the
 * rejection of its own deferred clipboard promise.
 */
if (typeof window !== 'undefined') window.addEventListener('unhandledrejection', event => {
  const reason = event.reason as { name?: unknown; message?: unknown } | undefined;
  if (reason?.name === 'Canceled' && reason.message === 'Canceled') event.preventDefault();
});
const exHandlers = new WeakMap<object, ExHandlers>();
let exCommandsDefined = false;
function defineExCommands(VimMode: unknown) {
 if (exCommandsDefined) return;
 exCommandsDefined = true;
 const vim = (VimMode as { Vim: { defineEx(name: string, prefix: string, run: (cm: VimAdapter, params: { argString?: string }) => void): void } }).Vim;
 vim.defineEx('write', 'w', cm => exHandlers.get(cm.editor)?.write(false));
 vim.defineEx('wq', 'wq', cm => exHandlers.get(cm.editor)?.write(true));
 vim.defineEx('xit', 'x', cm => exHandlers.get(cm.editor)?.write(true));
 vim.defineEx('quit', 'q', (cm, params) => exHandlers.get(cm.editor)?.quit(params.argString?.trim() === '!'));
}

// Editors showing the same workspace file share one model, so two editor
// groups on one file stay in step with independent cursors. Counted, so the
// model goes with its last editor.
const shared=new Map<string,{model:monaco.editor.ITextModel;users:number}>();
function acquireModel(key:string|undefined,path:string,value:string){
 if(!key){const model=monaco.editor.createModel(value,undefined,monaco.Uri.parse(`inmemory://workspace/${crypto.randomUUID()}/${path}`));return {model,release:()=>model.dispose()};}
 let entry=shared.get(key);
 if(!entry||entry.model.isDisposed()){entry={model:monaco.editor.createModel(value,undefined,monaco.Uri.parse(`inmemory://shared/${encodeURIComponent(key)}/${path}`)),users:0};shared.set(key,entry);}
 entry.users++;
 const held=entry;
 return {model:held.model,release:()=>{if(--held.users===0){held.model.dispose();if(shared.get(key)===held)shared.delete(key);}}};
}

// Where each view of a file was (cursor, scroll, folds), so switching tabs or
// reopening a file returns to the same place; and the last reveal it applied.
const viewStates=new Map<string,monaco.editor.ICodeEditorViewState>();
const applied=new Map<string,number>();

export type Reveal={line?:number;nonce:number};
/** Prose wraps by default, code does not; Alt+Z toggles it for that kind of file, remembered and applied to every open editor. */
const prose=(path:string)=>/\.(md|mdx|markdown|txt|text|rst|adoc|log)$/i.test(path)||!/\.[^/]+$/.test(path);
const wrapKey=(path:string)=>`lazybox:editor.word-wrap.${prose(path)?'prose':'code'}`;
const wrapFor=(path:string):'on'|'off'=>{let saved:string|null=null;try{saved=localStorage.getItem(wrapKey(path));}catch{/* a convenience */}return saved==='on'||saved==='off'?saved:prose(path)?'on':'off';};
const wrapEvent='lazybox:editor-word-wrap';
/** `onSave` resolves to false when the file was not saved. With `vim`, Esc
 * belongs to the editor (Vim users press it reflexively to be sure they are in
 * normal mode), and :q closes through the dialog's unsaved-changes guard. */
export default function CodeEditor({path,value,readOnly,line,vim=false,modelKey,viewKey,reveal,onChange,onSave,onQuit,onFocus}:{path:string;value:string;readOnly:boolean;line?:number;vim?:boolean;/** Share the text model with other editors of this key. */modelKey?:string;/** With modelKey: remember this view's place in the file (an editor group's id). */viewKey?:string;/** Put the cursor on a line, or just focus, once per nonce. */reveal?:Reveal;onChange(value:string):void;onSave():unknown;onQuit?(force:boolean):void;onFocus?():void}) {
 const container=useRef<HTMLDivElement>(null),status=useRef<HTMLDivElement>(null),instance=useRef<monaco.editor.IStandaloneCodeEditor>(null);
 const change=useRef(onChange),save=useRef(onSave),quit=useRef(onQuit),locked=useRef(readOnly),focused=useRef(onFocus);change.current=onChange;save.current=onSave;quit.current=onQuit;locked.current=readOnly;focused.current=onFocus;
 const vimActive=useRef(false);
 const [vimError,setVimError]=useState('');
 useEffect(()=>{
  const {model,release}=acquireModel(modelKey,path,value);
  if(model.getValue()!==value)replaceText(model,value);
  const editor=monaco.editor.create(container.current!,{model,theme:'lazybox',readOnly,automaticLayout:true,minimap:{enabled:false},...editorFont(),padding:{top:14,bottom:14},scrollBeyondLastLine:false,wordWrap:wrapFor(path),wrappingIndent:'same',renderLineHighlight:'line',tabSize:2,ariaLabel:'File source editor',fixedOverflowWidgets:true});
  instance.current=editor;
  const unfollow=subscribeAppearance(()=>editor.updateOptions(editorFont()));
  const view=modelKey&&viewKey?modelKey+'|'+viewKey:'';
  const subscription=editor.onDidChangeModelContent(()=>change.current(editor.getValue()));
  // An action, not addCommand: Monaco routes addCommand to the newest editor
  // when several are open, so a save would land in the wrong group.
  const saving=editor.addAction({id:'lazybox.save',label:'Save file',keybindings:[monaco.KeyMod.CtrlCmd|monaco.KeyCode.KeyS],run:()=>{save.current();}});
  const wrapping=editor.addAction({id:'lazybox.wordWrap',label:'Toggle word wrap',keybindings:[monaco.KeyMod.Alt|monaco.KeyCode.KeyZ],run:()=>{const next=editor.getOption(monaco.editor.EditorOption.wordWrap)==='off'?'on':'off';try{localStorage.setItem(wrapKey(path),next);}catch{/* a convenience */}window.dispatchEvent(new CustomEvent(wrapEvent,{detail:wrapKey(path)}));}});
  const applyWrap=(event:Event)=>{if((event as CustomEvent<string>).detail===wrapKey(path))editor.updateOptions({wordWrap:wrapFor(path)});};
  window.addEventListener(wrapEvent,applyWrap);
  const focus=editor.onDidFocusEditorText(()=>focused.current?.());
  const remembered=view?viewStates.get(view):undefined;
  if(remembered)editor.restoreViewState(remembered);
  if(line){editor.setPosition({lineNumber:line,column:1});editor.revealLineInCenter(line);}
  return()=>{unfollow();if(view){const state=editor.saveViewState();if(state)viewStates.set(view,state);}subscription.dispose();saving.dispose();wrapping.dispose();window.removeEventListener(wrapEvent,applyWrap);focus.dispose();editor.dispose();release();instance.current=null;};
 },[path,modelKey,viewKey]);
 useEffect(()=>{
  const editor=instance.current;if(!editor||!reveal)return;
  const view=(modelKey||path)+'|'+(viewKey||'');
  if(applied.get(view)===reveal.nonce)return;
  // Recorded after this commit: React's development double mount re-creates
  // the editor straight away, and the second editor must still get the reveal.
  // A later remount (switching back to the tab) must not.
  setTimeout(()=>applied.set(view,reveal.nonce),0);
  if(reveal.line){editor.setPosition({lineNumber:reveal.line,column:1});editor.revealLineInCenter(reveal.line);}
  editor.focus();
 },[reveal?.nonce,path,modelKey,viewKey]);
 useEffect(()=>{
  const editor=instance.current;if(!vim||!editor)return;
  let adapter:VimAdapter|undefined,disposed=false;setVimError('');
  // Loaded on demand: nobody without Vim editing pays for it.
  import('monaco-vim').then(({initVimMode,VimMode,StatusBar})=>{
   if(disposed||!status.current)return;
   defineExCommands(VimMode);
   // monaco-vim drives Monaco's textarea input. With Chromium's native
   // EditContext, keys typed just before Esc arrive after Vim has left insert
   // mode and are dropped or reordered, so Vim editing keeps the textarea.
   editor.updateOptions({editContext:false});
   // The same mode words as the message box, and no stale <Esc> in the key echo.
   class Bar extends StatusBar{
    setMode(event:{mode:string;subMode?:string}){this.setText(event.mode==='visual'&&event.subMode&&event.subMode!=='characterwise'?`VISUAL ${event.subMode==='linewise'?'LINE':'BLOCK'}`:event.mode.toUpperCase());}
    setKeyBuffer(keys:string){const end=keys.lastIndexOf('<Esc>');super.setKeyBuffer(end<0?keys:keys.slice(end+5));}
   }
   adapter=initVimMode(editor,status.current,Bar) as unknown as VimAdapter;
   vimActive.current=true;
   exHandlers.set(editor,{
    write(thenQuit){
     if(locked.current){adapter?.openNotification('Read-only · choose Edit file to change it');return;}
     void Promise.resolve(save.current()).then(saved=>{if(saved===false){adapter?.openNotification('Not saved · see the message above');return;}adapter?.openNotification(`"${path.split('/').at(-1)}" written`);if(thenQuit)quit.current?.(false);});
    },
    quit(force){quit.current?.(force);},
   });
  },error=>{if(!disposed)setVimError(`Vim editing could not load: ${String(error)}`);});
  return()=>{disposed=true;vimActive.current=false;exHandlers.delete(editor);if(adapter){adapter.dispose();editor.updateOptions({editContext:true});}};
 },[vim,path,modelKey,viewKey]);
 useEffect(()=>{const model=instance.current?.getModel();if(model&&model.getValue()!==value)replaceText(model,value);},[value]);
 useEffect(()=>{instance.current?.updateOptions({readOnly});},[readOnly]);
 // Cancelling the keydown keeps the dialog open; Vim and Monaco's own
 // widgets (suggest, find) still receive the Escape.
 const keepEscape=(event:KeyboardEvent)=>{if(event.key==='Escape'&&vimActive.current)event.preventDefault();};
 return <div className="code-editor-frame" onKeyDownCapture={vim?keepEscape:undefined}>
  <div ref={container} className="workspace-code-editor" data-testid="workspace-code-editor"/>
  {vim&&<div className="vim-status" aria-label="Vim status"><div ref={status} className="vim-status-bar" role="status"/>{vimError?<span className="vim-notification" role="alert">{vimError}</span>:<span className="vim-status-help">:w save · :q close · :wq save and close</span>}</div>}
 </div>;
}
