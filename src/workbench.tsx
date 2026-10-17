// Workbench: recent matters, running tasks and interrupted imports.
import React, { useState } from 'react';
import { api, applyChange, day, MATTER_TYPES, snapshot, statusLabel, type Matter } from './api';
import { useSession } from './main';
import { Busy, Empty, ErrorLine, Field, Modal, useAction, useLoad } from './ui';

const STAGES: Record<string, string> = {
  parse_pdf: '解析 PDF', orientation: '整页方向', unwarp: '弯曲矫正', detection: '文字检测', line_orientation: '行方向', recognition: '文字识别',
  index: '建立索引', embedding: '向量化', probe: '读取媒体信息', decode_audio: '解码音频', frames: '抽取画面', asr: '语音转写', backup: '备份',
};
const SKILLS: Record<string, string> = {
  'document-import': '导入资料', 'document-ocr': '识别文字', 'knowledge-index': '建立索引', 'audio-review': '音频转写', 'video-review': '视频识别',
  'lvjin-retrieval': '资料检索', 'contract-review': '合同阅读', 'loan-analysis': '借款核对', 'budget-analysis': '预算分析', 'media-decode': '媒体解码', 'audio-transcribe': '语音转写', 'image-review': '图片识别',
};
export const skillName = (id: string) => SKILLS[id] ?? id;

export function Workbench() {
  const { info, go, tasks, revision, refresh } = useSession();
  const root = useLoad(() => snapshot(info.root_matter), [info.root_matter, revision]);
  const interrupted = useLoad(() => api.view<{ selection_id: string; matter_id: string; display_name: string; updated_at: string }[]>('interrupted'), [revision]);
  const [creating, setCreating] = useState(false);
  const { run } = useAction();
  const matters = root.data?.matters ?? [];
  const active = tasks.filter((t) => t.status === 'running');
  return (
    <div className="page">
      <header className="page-head">
        <div>
          <p className="eyebrow">工作台</p>
          <h1>{info.name}</h1>
        </div>
        <div className="actions">
          <button className="primary" onClick={() => setCreating(true)}>新建事务</button>
          <button onClick={() => go({ name: 'search' })}>检索资料</button>
        </div>
      </header>
      <section className="panel">
        <h2>进行中的任务</h2>
        {active.length === 0 ? (
          <p className="muted">没有正在运行的任务。导入、识别与转写会在后台进行，可随时停止。</p>
        ) : (
          <ul className="task-list">
            {active.map((t) => (
              <li key={t.task_id}>
                <span className="task-name">{skillName(t.skill)}</span>
                <span className="task-stage">{t.progress ? `${skillName(t.progress.skill)} · ${STAGES[t.progress.stage] ?? t.progress.stage}` : '准备中'}</span>
                {t.progress && t.progress.total > 0 && <progress max={t.progress.total} value={t.progress.done} aria-label="进度" />}
                <button className="small" onClick={() => run(() => api.cancelTask(t.task_id), '已请求停止')}>停止</button>
              </li>
            ))}
          </ul>
        )}
      </section>
      {(interrupted.data?.length ?? 0) > 0 && (
        <section className="panel warn">
          <h2>中断的导入</h2>
          <p className="muted">这些资料在锁定或退出时尚未处理完。原件已加密保存，可从中断处继续，不会重复导入。</p>
          <ul className="plain">
            {interrupted.data!.map((item) => (
              <li key={item.selection_id} className="row-between">
                <span>{item.display_name}<span className="muted small">（{day(item.updated_at)}）</span></span>
                <button className="small" onClick={() => run(async () => { await api.resumeImport(item.selection_id, item.matter_id); refresh(); }, '已继续导入')}>继续导入</button>
              </li>
            ))}
          </ul>
        </section>
      )}
      <section className="panel">
        <div className="row-between">
          <h2>最近事务</h2>
          <button className="link" onClick={() => go({ name: 'matters' })}>全部事务</button>
        </div>
        {root.loading && !root.data ? <Busy /> : <ErrorLine text={root.error} />}
        {matters.length === 0 && !root.loading ? (
          <Empty title="还没有事务">新建一件事务（例如“房屋租赁”或“借款给朋友”），再把合同、借条、账单、截图或录音导入其中。</Empty>
        ) : (
          <div className="cards">
            {matters.slice(0, 6).map((m) => (
              <MatterCard key={m.id} matter={m} onOpen={() => go({ name: 'matter', id: m.id })} />
            ))}
          </div>
        )}
      </section>
      <section className="panel quiet">
        <h2>当前版本可以做什么</h2>
        <ul className="plain columns">
          <li>导入 PDF、Word、表格、文本、图片、截图、录音与视频，原件加密保存在本机。</li>
          <li>扫描件与照片自动识别文字，每行文字都能回到原图位置。</li>
          <li>录音转写为带时间段的文字，视频按画面抽帧识别文字。</li>
          <li>在所选事务中检索原文，按相关度重排并显示出处。</li>
          <li>校对候选金额与日期，按确认的记录进行确定性计算。</li>
          <li>导出 Markdown/CSV，创建加密备份并在新电脑恢复。</li>
        </ul>
        <p className="muted small">回答与分析需要你配置的生成接口，将在后续版本（M2）提供；当前版本不会把资料发送到任何地方。</p>
      </section>
      {creating && <NewMatter root={info.root_matter} onClose={() => setCreating(false)} onCreated={(id) => { setCreating(false); refresh(); go({ name: 'matter', id }); }} />}
    </div>
  );
}

export function MatterCard({ matter, onOpen }: { matter: Matter; onOpen: () => void }) {
  return (
    <button className="card matter-card" onClick={onOpen}>
      <span className="card-kind">{MATTER_TYPES[matter.matter_type] ?? matter.matter_type}</span>
      <strong>{matter.title}</strong>
      <span className="muted small">
        {matter.documents ?? 0} 份资料 · {matter.candidate_facts ?? 0} 条待校对 · 更新于 {day(matter.updated_at)}
      </span>
      {matter.status !== 'active' && <span className="muted small">{statusLabel(matter.status)}</span>}
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
    <Modal title="新建事务" onClose={onClose}>
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
          <button type="button" onClick={onClose}>取消</button>
          <button className="primary" disabled={busy || !title.trim()}>{busy ? '正在创建…' : '创建'}</button>
        </div>
      </form>
    </Modal>
  );
}
