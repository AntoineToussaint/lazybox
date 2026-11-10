import { useEffect, useRef } from 'react';
import * as monaco from 'monaco-editor';
import EditorWorker from 'monaco-editor/editor/editor.worker.js?worker';
import JsonWorker from 'monaco-editor/language/json/json.worker.js?worker';

import TsWorker from 'monaco-editor/language/typescript/ts.worker.js?worker';
import CssWorker from 'monaco-editor/language/css/css.worker.js?worker';
import HtmlWorker from 'monaco-editor/language/html/html.worker.js?worker';

globalThis.MonacoEnvironment = { getWorker: (_id, label) => label === 'json' ? new JsonWorker() : ['typescript','javascript'].includes(label) ? new TsWorker() : ['css','scss','less'].includes(label) ? new CssWorker() : ['html','handlebars','razor'].includes(label) ? new HtmlWorker() : new EditorWorker() };
monaco.editor.defineTheme('lazybox', { base: 'vs-dark', inherit: true, rules: [], colors: { 'editor.background':'#1b1c22', 'editorLineNumber.foreground':'#666976', 'editor.lineHighlightBackground':'#252630', 'editor.selectionBackground':'#50496a' } });

export default function CodeEditor({path,value,readOnly,line,onChange,onSave}:{path:string;value:string;readOnly:boolean;line?:number;onChange(value:string):void;onSave():void}) {
 const container=useRef<HTMLDivElement>(null),instance=useRef<monaco.editor.IStandaloneCodeEditor>(null);
 const change=useRef(onChange),save=useRef(onSave);change.current=onChange;save.current=onSave;
 useEffect(()=>{
  const uri=monaco.Uri.parse(`inmemory://workspace/${crypto.randomUUID()}/${path}`);
  const model=monaco.editor.createModel(value,undefined,uri);
  const editor=monaco.editor.create(container.current!,{model,theme:'lazybox',readOnly,automaticLayout:true,minimap:{enabled:false},fontSize:13,lineHeight:21,padding:{top:14,bottom:14},scrollBeyondLastLine:false,wordWrap:'off',renderLineHighlight:'line',tabSize:2,ariaLabel:'File source editor',fixedOverflowWidgets:true});
  instance.current=editor;
  const subscription=editor.onDidChangeModelContent(()=>change.current(editor.getValue()));
  editor.addCommand(monaco.KeyMod.CtrlCmd|monaco.KeyCode.KeyS,()=>save.current());
  if(line){editor.setPosition({lineNumber:line,column:1});editor.revealLineInCenter(line);}
  return()=>{subscription.dispose();editor.dispose();model.dispose();instance.current=null;};
 },[path]);
 useEffect(()=>{if(instance.current&&instance.current.getValue()!==value)instance.current.setValue(value);},[value]);
 useEffect(()=>{instance.current?.updateOptions({readOnly});},[readOnly]);
 return <div ref={container} className="workspace-code-editor" data-testid="workspace-code-editor"/>;
}
