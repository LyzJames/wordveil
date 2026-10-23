// Workbench (design 9.2): what needs you, what is running, recent matters.
import React, { useState } from 'react';
import { api, applyChange, MATTER_TYPES, relative, skillName, stageName, statusLabel, type Matter, type TaskState } from './api';
import { Icon } from './icons';
import { useSession } from './session';
import { Callout, Empty, Field, Modal, Skeleton, useAction, useLoad } from './ui';

type Interrupted = { selection_id: string; matter_id: string; display_name: string; updated_at: string };

export function Workbench() {
  const { info, go, tasks, revision, refresh, matters, mattersLoaded, due, connection, newMatter } = useSession();
  const interrupted = useLoad(() => api.view<Interrupted[]>('interrupted'), [revision]);
  const indexJobs=useLoad(api.indexJobs,[revision]);
  const { run } = useAction();
  const [, tick] = useState(0);
  const active = tasks.filter((t) => t.status === 'running');
  // Tasks are listed newest first: the two most recent failures.
  const failed = tasks.filter((t) => t.status === 'failed' && t.envelope).slice(0, 2);
  const pending = matters.filter((m) => (m.candidate_facts ?? 0) > 0);
  const recent = [...matters].sort((a, b) => b.updated_at.localeCompare(a.updated_at)).slice(0, 6);
  const loaded = interrupted.data !== undefined && indexJobs.data!==undefined && mattersLoaded;
  const nothing = pending.length === 0 && (interrupted.data?.length ?? 0) === 0 && (indexJobs.data?.length??0)===0 && failed.length === 0 && due.length === 0;

  return (
    <div className="page fade-in">
      <header className="page-head">
        <div className="titles">
          <div className="text-secondary">{info.name} · 本机加密</div>
          <h1 className="t-page">工作台</h1>
        </div>
        <div className="actions">
          <button className="btn btn-primary" onClick={newMatter}>
            <Icon name="add" />
            新建事务
          </button>
          <button className="btn" onClick={() => go({ name: 'qa' })}>
            <Icon name="chat" />
            提问
          </button>
        </div>
      </header>

      {matters.length === 0 && loaded ? (
        <section className="section start-card card" aria-labelledby="start">
          <h2 id="start" className="t-subtitle">开始使用</h2>
          <ol className="start-steps">
            <li>
              <strong>新建第一件事务</strong>
              <span className="text-secondary">例如“房屋租赁”或“借款给朋友”，同一件事的资料、事实和金额归在一起。</span>
              <button className="btn btn-primary btn-sm" onClick={newMatter}>新建事务</button>
            </li>
            <li>
              <strong>导入资料</strong>
              <span className="text-secondary">合同、借条、账单、截图、录音或视频；原件加密保存在本机，自动识别文字并建立检索。</span>
            </li>
            <li>
              <strong>（可选）连接生成接口</strong>
              <span className="text-secondary">连接后可以生成带引用、逐句核查的回答；发送任何资料前都会先请你确认。</span>
              {!connection && <button className="btn btn-sm" onClick={() => go({ name: 'connections' })}>连接生成接口</button>}
            </li>
          </ol>
        </section>
      ) : (
        <>
          <section className="section" aria-labelledby="todo">
            <div className="section-head">
              <h2 id="todo">需要你处理</h2>
            </div>
            <div className="stack reading">
              {!loaded && <Skeleton rows={2} />}
              {pending.map((m) => (
                <Callout
                  key={m.id}
                  tone="attention"
                  title={`${m.title} 有 ${m.candidate_facts} 条候选待校对`}
                  action={<button className="btn btn-sm" onClick={() => go({ name: 'matter', id: m.id, tab: 'facts' })}>去核对</button>}
                >
                  金额与日期由规则从原文提取，确认后才会用于计算。
                </Callout>
              ))}
              {interrupted.data?.map((item) => (
                <Callout
                  key={item.selection_id}
                  tone="warning"
                  title={`《${item.display_name}》的导入在锁定或退出时中断`}
                  action={
                    <button className="btn btn-sm" onClick={() => run(async () => { await api.resumeImport(item.selection_id, item.matter_id); refresh(); }, '已继续导入')}>
                      继续导入
                    </button>
                  }
                >
                  原件已加密保存，可以从中断处继续，不会重复导入（{relative(item.updated_at)}）。
                </Callout>
              ))}
              {indexJobs.data?.map((job)=><Callout key={job.id} tone="warning" title={`《${job.display_name}》${job.running?'正在创建索引':'的索引可继续'}`} action={<button className="btn btn-sm" disabled={job.running} onClick={()=>job.public?go({name:'knowledge'}):run(async()=>{await api.indexResume(job.id);refresh();},'已从保存的批次继续索引')}>{job.public?'在知识中心继续公共包':'继续索引'}</button>}>已完成 {job.completed}/{job.total} 个片段向量。来源与模型空间未变化时会复用已完成的批次；完整后才发布新索引。</Callout>)}
              {due.length > 0 && (
                <ul className="due-list" aria-label="即将到期的待办">
                  {due.map((t) => {
                    const days = daysUntil(t.due_on);
                    return (
                      <li key={t.id}>
                        <span className="ellipsis" title={`${t.title} · ${t.matter_title}`}>
                          <Icon name="calendar" /> <strong>{t.title}</strong>
                          <span className="t-caption text-secondary"> · {t.matter_title}</span>
                        </span>
                        <span className={`t-caption ${days < 0 ? 'overdue' : 'text-secondary'}`}>
                          {days < 0 ? `已逾期 ${-days} 天` : days === 0 ? '今天到期' : `${days} 天后到期`}（{t.due_on}）
                        </span>
                        <button className="btn btn-sm" onClick={() => go({ name: 'matter', id: t.matter_id, tab: 'timeline' })}>查看</button>
                      </li>
                    );
                  })}
                </ul>
              )}
              {failed.map((t) => (
                <Callout key={t.task_id} tone="danger" title={`${skillName(t.skill)}失败`} action={<RetryButton task={t} />}>
                  {t.envelope!.warnings.map((w) => w.message).join('；') || statusLabel(t.status)}
                </Callout>
              ))}
              {loaded && nothing && <p className="text-secondary">没有需要你处理的事项。</p>}
            </div>
          </section>

          <section className="section" aria-labelledby="running">
            <div className="section-head">
              <h2 id="running">进行中</h2>
            </div>
            {active.length === 0 ? (
              <p className="text-secondary">没有进行中的任务。导入、识别与转写在后台进行，可以随时停止。</p>
            ) : (
              <div className="stack reading">
                {active.map((t) => {
                  const pct = t.progress && t.progress.total > 0 ? Math.round((t.progress.done / t.progress.total) * 100) : null;
                  return (
                    <div key={t.task_id} className="card task-card">
                      <div style={{ minWidth: 0 }}>
                        <div className="t-strong ellipsis">{skillName(t.skill)}</div>
                        <div className="t-caption text-secondary">
                          {t.progress ? `${stageName(t.progress.stage)}${t.progress.total > 0 ? ` · ${t.progress.done}/${t.progress.total}` : ''}` : '准备中'} · 开始于 {relative(t.started_at)}
                        </div>
                      </div>
                      <button className="btn btn-sm" onClick={() => run(() => api.cancelTask(t.task_id), '已请求停止').then(() => tick((x) => x + 1))}>
                        <Icon name="stop" />
                        停止
                      </button>
                      <div className="progress" aria-label="进度">
                        <span style={{ width: `${pct ?? 6}%` }} />
                      </div>
                    </div>
                  );
                })}
              </div>
            )}
          </section>

          <section className="section" aria-labelledby="recent">
            <div className="section-head">
              <h2 id="recent">最近事务</h2>
              <button className="btn btn-link" onClick={() => go({ name: 'matters' })}>全部事务</button>
            </div>
            <div className="grid-cards">
              {recent.map((m) => (
                <MatterCard key={m.id} matter={m} onOpen={() => go({ name: 'matter', id: m.id })} />
              ))}
            </div>
          </section>
        </>
      )}
    </div>
  );
}

/** Whole days from today (local time) to an ISO date; negative when overdue. */
function daysUntil(iso: string): number {
  const [y, m, d] = iso.slice(0, 10).split('-').map(Number);
  const today = new Date();
  return Math.round((new Date(y, m - 1, d).getTime() - new Date(today.getFullYear(), today.getMonth(), today.getDate()).getTime()) / 86400000);
}

/** Start a failed or stopped local task again with its original input (the
 *  core keeps the input of tasks started from the interface; imports are
 *  idempotent per file selection). */
export function RetryButton({ task }: { task: TaskState }) {
  const { busy, run } = useAction();
  const [started, setStarted] = useState(false);
  if (!task.retryable || started) return null;
  return (
    <button
      className="btn btn-xs"
      disabled={busy}
      onClick={async () => {
        if ((await run(() => api.retryTask(task.task_id), '已重新开始')) !== undefined) setStarted(true);
      }}
    >
      <Icon name="refresh" />
      重试
    </button>
  );
}

export function MatterCard({ matter, onOpen }: { matter: Matter; onOpen: () => void }) {
  const pending = matter.candidate_facts ?? 0;
  return (
    <button className="card interactive matter-card" onClick={onOpen}>
      <span className="card-kind">
        <Icon name="folder" />
        {MATTER_TYPES[matter.matter_type] ?? matter.matter_type}
        {matter.status !== 'active' && ` · ${statusLabel(matter.status)}`}
      </span>
      <span className="card-title">{matter.title}</span>
      <span className="card-meta">
        <span>{matter.documents ?? 0} 份资料</span>
        <span className={pending ? 'pending' : undefined}>{pending ? `${pending} 条待校对` : '无待校对'}</span>
        <span title={matter.updated_at}>更新于 {relative(matter.updated_at)}</span>
      </span>
    </button>
  );
}

export function NewMatter({ root, onClose, onCreated }: { root: string; onClose: () => void; onCreated: (id: string) => void }) {
  const [title, setTitle] = useState('');
  const [type, setType] = useState('rental');
  const { busy, run } = useAction();
  async function submit(e: React.FormEvent) {
    e.preventDefault();
    const done = await run(() => applyChange(root, { type: 'create_matter', title: title.trim(), matter_type: type }));
    if (done) onCreated(done.evidence_refs[0]);
  }
  return (
    <Modal title="新建事务" icon="folder" onClose={onClose}>
      <form className="stack" onSubmit={submit}>
        <Field label="事务名称" hint="例如：浦东租房合同、借给方宁的钱、2026 家庭开支">
          <input value={title} maxLength={120} onChange={(e) => setTitle(e.target.value)} required autoFocus />
        </Field>
        <Field label="类型">
          <select value={type} onChange={(e) => setType(e.target.value)}>
            {Object.entries(MATTER_TYPES).map(([id, label]) => (
              <option key={id} value={id}>{label}</option>
            ))}
          </select>
        </Field>
        <div className="actions end">
          <button type="button" className="btn" onClick={onClose}>取消</button>
          <button className="btn btn-primary" disabled={busy || !title.trim()}>{busy ? '正在创建…' : '创建'}</button>
        </div>
      </form>
    </Modal>
  );
}

export function MattersEmpty() {
  const { newMatter } = useSession();
  return (
    <Empty icon="folder" title="还没有事务" action={<button className="btn btn-primary" onClick={newMatter}>新建事务</button>}>
      事务用来归集同一件事的资料、事实、金额记录与待办。
    </Empty>
  );
}
