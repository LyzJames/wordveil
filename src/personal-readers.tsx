import React,{useEffect,useRef,useState} from 'react';
import {api,type HistoryEntry,type RetrievalResult,type VersionPages} from './api';
import {invoke} from '@tauri-apps/api/core';
import {ExportButton} from './export';

export function LegacyHistory({search}:{search:string}){
 const [items,setItems]=useState<HistoryEntry[]>([]),[selected,setSelected]=useState<{id:string;title:string;text:string}|null>(null),[error,setError]=useState('');
 useEffect(()=>{let active=true;api.qaHistory({search,limit:20}).then(v=>{if(active)setItems(v.items);}).catch(e=>{if(active)setError(String(e));});return()=>{active=false;};},[search]);
 if(!items.length&&!error)return null;
 return <section><h2>旧版历史（只读）</h2>{error&&<p role="status">{error}</p>}{items.map(item=><button className="sy-row" key={item.id} onClick={async()=>{try{const result=await api.result<RetrievalResult>(item.id);const answer=result.answer;const lines:string[]=[];if(answer?.sections){for(const section of Object.values(answer.sections)){if(Array.isArray(section))for(const row of section){if(typeof row==='string')lines.push(row);else if(row&&typeof row==='object'&&'text'in row)lines.push(String(row.text));}}}setSelected({id:item.id,title:item.question,text:lines.join('\n\n')||JSON.stringify(result,null,2)});}catch(e){setError(String(e));}}}>{item.question}</button>)}{selected&&<Reader title={selected.title} onClose={()=>setSelected(null)}><p className="sy-muted">这是原版保存的记录，不会重新运行旧工作流。</p><ExportButton small name={selected.title} resultRefs={async()=>[selected.id]}/><div className="sy-original">{selected.text}</div></Reader>}</section>;
}
export function FileReader({id,name,onClose}:{id:string;name:string;onClose:()=>void}){
 const [data,setData]=useState<VersionPages|null>(null),[error,setError]=useState('');
 useEffect(()=>{let active=true;api.view<VersionPages>('version',id).then(v=>{if(active)setData(v);}).catch(e=>{if(active)setError(String(e));});return()=>{active=false;};},[id]);
 return <Reader title={name} onClose={onClose}>{error?<p role="status">{error}</p>:!data?<p>正在本机读取…</p>:<><p className="sy-muted">原文仅在本机显示。{data.pages.length} 页，{data.blocks.length} 个文字块。</p>{data.blocks.map(row=><div className="sy-original" key={row.block}>{row.text}</div>)}{!data.blocks.length&&<p>尚未提取文字；可将资料加入对话，由本机 OCR / 转写处理。</p>}</>}</Reader>;
}
export function LegacyProject({id}:{id:string}){
 const [text,setText]=useState('');
 useEffect(()=>{setText('');},[id]);
 return <details className="sy-sources"><summary onClick={()=>{if(!text)invoke('personal_query',{operation:'legacy_project',request:{id}}).then(v=>setText(JSON.stringify(v,null,2))).catch(e=>setText(String(e)));}}>查看原版项目记录（只读）</summary><ExportButton small name="旧版项目记录" resultRefs={async()=>{const e=await api.run('matter-manage',{matter_id:id,operation:'read'});if(!e.data_ref)throw new Error('无法准备历史导出');return [e.data_ref];}}/><pre>{text||'正在读取…'}</pre></details>;
}
function Reader({title,onClose,children}:{title:string;onClose:()=>void;children:React.ReactNode}){
 const dialog=useRef<HTMLDialogElement>(null);
 useEffect(()=>{dialog.current?.showModal();return()=>dialog.current?.close();},[]);
 return <dialog ref={dialog} className="sy-reader" onCancel={e=>{e.preventDefault();onClose();}}><header><strong>{title}</strong><button autoFocus onClick={onClose} aria-label="关闭阅读器">×</button></header><div>{children}</div></dialog>;
}
