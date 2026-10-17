import React, { useState } from 'react';
import { createRoot } from 'react-dom/client';
import { invoke } from '@tauri-apps/api/core';
import './style.css';

type Status={stage:string;version:string;contracts:number;roles:number;resources_verified:boolean;models:string[];cpu_backend:string};
function App(){
  const [status,setStatus]=useState<Status|null>(null);
  const [error,setError]=useState('');
  const [checking,setChecking]=useState(false);
  async function check(){setChecking(true);setError('');try{setStatus(await invoke<Status>('installation_status'));}catch(e){setError(String(e));}finally{setChecking(false);}}
  return <main>
    <header><div className="mark">律</div><div><p className="eyebrow">本地法律与金融工作台</p><h1>律金智库</h1></div><span className="stage">M0 验证原型</span></header>
    <section className="intro"><p className="eyebrow">开发阶段 · 0.1.0</p><h2>合同与离线运行验证</h2><p>本阶段验证执行合同、进程隔离、九个内置辅助模型与完整离线安装。业务工作流将在后续阶段实现。</p><button onClick={check} disabled={checking}>{checking?'正在校验…':'检查安装资源'}</button>{error&&<p role="alert" className="error">{error}</p>}</section>
    <section className="cards"><article><strong>{status?.contracts??'30'}</strong><h3>固定技能合同</h3><p>严格字段、权限边界和根预算。</p></article><article><strong>{status?.roles??'7'}</strong><h3>有界角色</h3><p>共享可信核心与模型所有者。</p></article><article><strong>9</strong><h3>内置辅助模型</h3><p>CPU 可运行，随完整介质安装。</p></article></section>
    {status&&<section className="verified"><span className="dot"/><div><h3>安装合同资源校验通过</h3><p>版本 {status.version} · {status.cpu_backend} · {status.contracts} 个技能合同、{status.roles} 个角色</p><p>模型运行验收与资源校验分别记录在 M0 报告中。</p></div></section>}
    <footer>本机原型 · 当前阶段 M0</footer>
  </main>;
}
createRoot(document.getElementById('root')!).render(<App/>);
