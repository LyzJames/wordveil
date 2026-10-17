// Export: a real preview first; writing needs a destination picked in the
// native save dialog (the trusted confirmation for an export).
import React, { useEffect, useState } from 'react';
import { api, ensure } from './api';
import { Busy, ErrorLine, Modal, useAction } from './ui';

export function ExportButton({ resultRefs, name, label = '导出' }: { resultRefs: () => Promise<string[]>; name: string; label?: string }) {
  const [refs, setRefs] = useState<string[] | null>(null);
  const { busy, run } = useAction();
  return (
    <>
      <button disabled={busy} onClick={async () => { const r = await run(resultRefs); if (r) setRefs(r); }}>{label}</button>
      {refs && <ExportModal refs={refs} name={name} onClose={() => setRefs(null)} />}
    </>
  );
}

function ExportModal({ refs, name, onClose }: { refs: string[]; name: string; onClose: () => void }) {
  const [format, setFormat] = useState<'markdown' | 'csv'>('markdown');
  const [draft, setDraft] = useState<{ id: string; preview: string; bytes: number } | null>(null);
  const [error, setError] = useState('');
  const [done, setDone] = useState('');
  const { busy, run } = useAction();
  useEffect(() => {
    let alive = true;
    setDraft(null);
    setError('');
    api
      .run('report-export', { result_refs: refs, format, operation: 'prepare' })
      .then(async (envelope) => {
        const e = ensure(envelope);
        const payload = await api.result<{ preview: string; bytes: number }>(e.data_ref!);
        if (alive) setDraft({ id: e.data_ref!, preview: payload.preview, bytes: payload.bytes });
      })
      .catch((err: unknown) => alive && setError(err instanceof Error ? err.message : String(err)));
    return () => {
      alive = false;
    };
  }, [refs, format]);
  async function save() {
    if (!draft) return;
    const outcome = await run(() => api.exportCommit(draft.id, refs, format, name));
    if (outcome && outcome.status === 'completed') setDone('已写入所选位置，并核对了文件内容哈希。');
  }
  return (
    <Modal title="导出预览" onClose={onClose} wide>
      <div className="stack">
        <div className="segmented" role="radiogroup" aria-label="格式">
          <button type="button" role="radio" aria-checked={format === 'markdown'} className={format === 'markdown' ? 'on' : ''} onClick={() => setFormat('markdown')}>Markdown 文档</button>
          <button type="button" role="radio" aria-checked={format === 'csv'} className={format === 'csv' ? 'on' : ''} onClick={() => setFormat('csv')}>CSV 表格</button>
        </div>
        <ErrorLine text={error} />
        {!draft && !error && <Busy label="正在生成预览…" />}
        {draft && (
          <>
            <pre className="preview" aria-label="导出内容预览">{draft.preview.replace(/^﻿/, '')}</pre>
            <p className="muted small">
              共 {draft.bytes} 字节{draft.preview.length < draft.bytes ? '，预览只显示开头部分' : ''}。CSV 中以 = + − @ 等开头的文字会加前导撇号，防止表格软件执行公式。候选内容未经确认，不构成法律或财务结论。
            </p>
          </>
        )}
        {done && <p className="ok-line">{done}</p>}
        <div className="actions end">
          <button onClick={onClose}>关闭</button>
          <button className="primary" disabled={!draft || busy || !!done} onClick={save}>选择保存位置并导出…</button>
        </div>
      </div>
    </Modal>
  );
}
