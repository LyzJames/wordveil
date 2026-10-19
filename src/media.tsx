// Media analysis entries (multimodal design 2, 4, 6): pick the pages of a
// scanned document or photo, or a range of a recording or video; the core
// prepares exactly what may be uploaded, the user approves it, and the result
// appears in the Q&A thread with the local comparison.
import React, { useState } from 'react';
import { api, type AnswerPreview, type DocumentRow, type VersionPages } from './api';
import { EgressDialog } from './egress';
import { Icon } from './icons';
import { useSession } from './session';
import { newTurnId, type TurnKind } from './turns';
import { Busy, Callout, ErrorLine, Field, Modal, useLoad } from './ui';

const QUESTIONS: Record<'image' | 'audio' | 'video', string> = {
  image: '这几页上的金额、日期、签名和关键约定是什么？',
  audio: '录音里提到了哪些金额、日期和约定？',
  video: '视频里出现了哪些单据、金额和说法？',
};

export function MediaAnalysisButton({ doc, matter }: { doc: DocumentRow; matter: string }) {
  const [open, setOpen] = useState(false);
  const kind = doc.media_kind === 'video' ? 'video' : doc.media_kind === 'audio' ? 'audio' : 'image';
  if (kind === 'image' && doc.pages === 0) return null;
  return (
    <>
      <button className="btn btn-xs" onClick={() => setOpen(true)} title={kind === 'image' ? '让生成接口直接看图，并与本机识别文字核对' : '本机转写后，让生成接口直接读取所选时段并核对'}>
        {kind === 'image' ? '看图分析' : '分析'}
      </button>
      {open && <MediaAnalysisModal doc={doc} matter={matter} kind={kind} onClose={() => setOpen(false)} />}
    </>
  );
}

function MediaAnalysisModal({ doc, matter, kind, onClose }: { doc: DocumentRow; matter: string; kind: 'image' | 'audio' | 'video'; onClose: () => void }) {
  const { connection, qa, go } = useSession();
  const pages = useLoad(() => (kind === 'image' ? api.view<VersionPages>('version', doc.version_id) : Promise.resolve(null)), [doc.version_id]);
  const [question, setQuestion] = useState(QUESTIONS[kind]);
  const [chosen, setChosen] = useState<Set<string> | null>(null);
  const [range, setRange] = useState({ start: '0', end: '' });
  const [preview, setPreview] = useState<AnswerPreview | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const pageIds = pages.data?.pages.map((p) => p.page_id) ?? [];
  const selected = chosen ?? new Set(pageIds.slice(0, 3));

  async function prepare() {
    setError('');
    setBusy(true);
    try {
      const request = kind === 'image'
        ? { question, image_refs: [...selected], connection: connection?.id ?? null }
        : { question, source_ref: doc.media_id, range: { start_s: Number(range.start) || 0, end_s: range.end ? Number(range.end) : null }, connection: connection?.id ?? null };
      setPreview(await api.mediaPreview(`${kind}-review` as 'image-review' | 'audio-review' | 'video-review', request));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  async function approve(excluded: string[], excludedMedia: string[]) {
    if (!preview) return;
    try {
      const task = await api.answerStart(preview.grant_id, excluded, excludedMedia);
      qa.update((turns) => [...turns, { id: newTurnId(), question, matters: [matter], kind: kind as TurnKind, state: 'running', started: Date.now(), task, source: doc.display_name }]);
      onClose();
      go({ name: 'qa' });
    } catch (e) {
      setPreview(null);
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  if (preview) {
    return <EgressDialog preview={preview} onCancel={() => { api.answerDiscard(preview.grant_id).catch(() => {}); setPreview(null); }} onConfirm={approve} />;
  }
  const flip = (id: string) => {
    const next = new Set(selected);
    if (next.has(id)) next.delete(id);
    else if (next.size < 12) next.add(id);
    setChosen(next);
  };
  return (
    <Modal
      title={kind === 'image' ? `看图分析：${doc.display_name}` : `${kind === 'video' ? '视频' : '录音'}分析：${doc.display_name}`}
      icon={kind === 'image' ? 'photo' : kind === 'video' ? 'video' : 'mic'}
      onClose={onClose}
      footer={
        <>
          <button className="btn" onClick={onClose}>取消</button>
          <button className="btn btn-primary" disabled={!connection || busy || !question.trim() || (kind === 'image' && selected.size === 0)} onClick={prepare}>
            {busy ? '正在本机准备…' : '预览将发送的内容'}
          </button>
        </>
      }
    >
      {!connection && (
        <Callout tone="attention" title="需要先连接生成接口" action={<button className="btn btn-sm" onClick={() => { onClose(); go({ name: 'connections' }); }}>连接生成接口</button>}>
          没有生成接口时，可以在“查看”中阅读本机识别与转写的文字。
        </Callout>
      )}
      <Field label="你想了解什么">
        <textarea rows={2} value={question} onChange={(e) => setQuestion(e.target.value)} maxLength={2000} />
      </Field>
      {kind === 'image' && (
        <fieldset className="pick">
          <legend>要直接读取的页面（最多 12 页）</legend>
          {pages.loading && <Busy label="正在读取页面…" />}
          {pages.data?.pages.map((p) => (
            <label key={p.page_id} className="check">
              <input type="checkbox" checked={selected.has(p.page_id)} onChange={() => flip(p.page_id)} />
              第 {p.page_no} 页{p.ocr_status === 'done' ? '' : '（尚未识别文字）'}
            </label>
          ))}
        </fieldset>
      )}
      {kind !== 'image' && (
        <div className="grid-2">
          <Field label="从（秒）"><input inputMode="decimal" value={range.start} onChange={(e) => setRange({ ...range, start: e.target.value })} /></Field>
          <Field label="到（秒，留空到结尾）"><input inputMode="decimal" value={range.end} onChange={(e) => setRange({ ...range, end: e.target.value })} /></Field>
        </div>
      )}
      <p className="t-caption text-secondary">
        <Icon name="shield" /> {kind === 'image'
          ? '本机先识别文字；生成接口通过图片能力自检时，会先在不提供识别文字的情况下直接看图，再与识别结果逐项核对。'
          : '本机先转写（视频还会抽帧识别）；生成接口通过对应能力自检时，再直接读取所选时段的画面或音频（分开发送，不发送完整视频）。'}
        预览会列出将发送的全部内容。
      </p>
      <ErrorLine text={error} />
    </Modal>
  );
}
