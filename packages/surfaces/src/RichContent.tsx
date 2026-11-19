import { fileReference, type FileLink } from './file-reference';
import { preloadEditor } from './editor-loader';
import { isMac } from './workbench';
import { Children, isValidElement, memo, useContext, useMemo, useState, type ReactNode } from 'react';
import { MermaidDiagram } from './MermaidDiagram';
import Markdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import hljs from 'highlight.js/lib/core';
import json from 'highlight.js/lib/languages/json';
import bash from 'highlight.js/lib/languages/bash';
import typescript from 'highlight.js/lib/languages/typescript';
import python from 'highlight.js/lib/languages/python';
import go from 'highlight.js/lib/languages/go';
import rust from 'highlight.js/lib/languages/rust';
import yaml from 'highlight.js/lib/languages/yaml';
import diff from 'highlight.js/lib/languages/diff';
import './rich-content.css';
import { contextMenu } from './ContextMenu';
import { codeBlockItems, fileLinkItems } from './chat-context-menu';
import type { OpenModifiers } from './file-links';
import { inlineRunItems, useRunnableBlock, useRunScope } from './code-runs';
import { inlineCommand, pathProgramsIn, programAt } from './code-run';
import { ConversationSourceContext, conversationSourceFromLink } from './conversation-source';
import { headingSlug } from './docs-model';
for (const [name, language] of Object.entries({json,bash,typescript,python,go,rust,yaml,diff})) hljs.registerLanguage(name,language);
hljs.registerAliases(['js','javascript','tsx','jsx','ts'],{languageName:'typescript'});

function Copy({text}:{text:string}) {
 const [state,setState]=useState('Copy');
 return <button type="button" aria-label="Copy code" onClick={()=>void navigator.clipboard.writeText(text).then(()=>setState('Copied'),()=>setState('Copy failed'))}>{state}</button>;
}
/** `runnable`: a block of a message's Markdown, which can offer Run when it is a shell block in an agent's message (code-runs.tsx). */
export const CodeBlock=memo(function CodeBlock({text,language='',label,runnable=false}:{text:string;language?:string;label?:string;runnable?:boolean}) {
 const highlighted=useMemo(()=>text.length<=100000&&hljs.getLanguage(language)?hljs.highlight(text,{language,ignoreIllegals:true}).value:undefined,[text,language]);
 const run=useRunnableBlock(text,language,runnable);
 const name=label||language||'Text';
 return <div className={'code-block'+(run?' is-runnable':'')} tabIndex={run?0:undefined} role={run?'group':undefined} aria-label={run?`${name} commands`:undefined} aria-keyshortcuts={run?'Enter Alt+Enter':undefined} onKeyDown={run?.onKeyDown}
  onContextMenu={contextMenu('code-block',()=>run?[...run.menu(),'separator',...codeBlockItems(text,language)]:codeBlockItems(text,language),{text,language})}>
  <header><span>{name}</span>{run?<span className="code-block-actions">{run.header}<Copy text={text}/></span>:<Copy text={text}/>}</header>
  <pre aria-label={label||`${language||'Plain text'} code`}><code>{highlighted===undefined?text:<span dangerouslySetInnerHTML={{__html:highlighted}}/>}</code></pre>
  {run?.footer}</div>;
});
const mermaidSource=(text:string)=><CodeBlock text={text} language="mermaid"/>;
function codeText(node:ReactNode):string {if(typeof node==='string')return node;if(Array.isArray(node))return node.map(codeText).join('');return '';}
const components={
 pre:({children}:{children?:ReactNode})=>{
  const child=Children.toArray(children)[0];
  if(!isValidElement<{className?:string;children?:ReactNode}>(child))return <pre>{children}</pre>;
  const language=/language-([^\s]+)/.exec(child.props.className||'')?.[1]?.toLowerCase()||'';
  const text=codeText(child.props.children).replace(/\n$/,'');
  return language==='mermaid'?<MermaidDiagram text={text} sourceView={mermaidSource}/>:<CodeBlock text={text} language={language} runnable/>;
 },
 a:({href,children}:{href?:string;children?:ReactNode})=>href&&/^https?:\/\//i.test(href)?<a href={href} target="_blank" rel="noopener noreferrer">{children}</a>:<span className="content-reference">{children}</span>,
 // Do not load external images from model output just by viewing a message.
 img:({alt}:{alt?:string})=><span className="content-reference">{alt||'Image reference'}</span>,
};
const fileTitle=`Open file · ${isMac?'⌥':'Alt+'}click for a quick look · ${isMac?'⌘':'Ctrl+'}click to open beside`;
/** All agent-authored prose uses this renderer, including help and planning.
 * Raw HTML is never interpreted; code and diagram source remain copyable. */
/** `onOpenFile` gets the click's modifiers: Alt asks for a quick look, Cmd/Ctrl for the file beside the current one. */
/**
 * A document the app ships (the in-app documentation, DocsViewer.tsx): its
 * relative links and anchors stay inside the viewer, its images are the
 * bundled files, and headings get GitHub-style ids for anchors to land on.
 */
export type RichDocument = { link(href: string): void; image(src: string): string | undefined };
const headingText=(node:ReactNode):string=>typeof node==='string'||typeof node==='number'?String(node):Array.isArray(node)?node.map(headingText).join(''):isValidElement<{children?:ReactNode}>(node)?headingText(node.props.children):'';
function documentComponents(doc:RichDocument){
 const heading=(Tag:'h1'|'h2'|'h3'|'h4')=>({children}:{children?:ReactNode})=><Tag id={headingSlug(headingText(children))}>{children}</Tag>;
 return {
  h1:heading('h1'),h2:heading('h2'),h3:heading('h3'),h4:heading('h4'),
  a:({href,children}:{href?:string;children?:ReactNode})=>!href?<span>{children}</span>:/^(https?:|mailto:)/i.test(href)?components.a({href,children}):<a href={href} className="doc-link" onClick={event=>{event.preventDefault();doc.link(href);}}>{children}</a>,
  img:({src,alt}:{src?:string;alt?:string})=>{const url=typeof src==='string'?doc.image(src):undefined;return url?<img src={url} alt={alt||''} loading="lazy"/>:<span className="content-reference">{alt||'Image'}</span>;},
 };
}
export const RichContent=memo(function RichContent({text,onOpenFile,document:doc}:{text:string;onOpenFile?(reference:FileLink,how:OpenModifiers):void;document?:RichDocument}) {
 const fileMenu=(file:FileLink)=>contextMenu('file-link',()=>fileLinkItems(file,onOpenFile!),file);
 // In an agent's message, inline code that reads as a command offers to run it, from its menu only.
 const run=useRunScope();
 const openSource = useContext(ConversationSourceContext);
 const programs=useMemo(()=>run?pathProgramsIn(text):[],[run,text]);
 const rawJSON=useMemo(()=>{if(doc||text.length>200000||!/^\s*[\[{]/.test(text))return false;try{JSON.parse(text);return true;}catch{return false;}},[text,doc]);
 const docComponents=useMemo(()=>doc?documentComponents(doc):undefined,[doc]);
 if(docComponents)return <div className="rich-content rich-document"><Markdown remarkPlugins={[remarkGfm]} components={{...components,...docComponents}}>{text}</Markdown></div>;
 return <div className="rich-content">{rawJSON?<CodeBlock text={text} language="json"/>:<Markdown remarkPlugins={[remarkGfm]} components={{...components,
 a:({href,children})=>{
  const source = openSource && href && conversationSourceFromLink(href);
  if (source) return <a href={href} onClick={event => { event.preventDefault(); openSource(source); }}>{children}</a>;
  const file=href&&fileReference(href);return file&&onOpenFile?<button className="file-reference" title={fileTitle} onPointerEnter={preloadEditor} onClick={event=>onOpenFile(file,event)} onContextMenu={fileMenu(file)}>{children}</button>:components.a({href,children});},
 code:({children,className,node})=>{const file=!className&&typeof children==='string'?fileReference(children,'automatic'):undefined;if(file&&onOpenFile)return <button className="file-reference inline-file" title={fileTitle} onPointerEnter={preloadEditor} onClick={event=>onOpenFile(file,event)} onContextMenu={fileMenu(file)}>{children}</button>;
  const command=run&&!className&&typeof children==='string'?inlineCommand(children,programAt(programs,node?.position?.start.offset)):'';
  return <code className={className} onContextMenu={command?contextMenu('inline-code',()=>inlineRunItems(run!.host,run!.messageId,command,run!.text),{text:children,command}):undefined}>{children}</code>;}
 }}>{text}</Markdown>}</div>;
});
