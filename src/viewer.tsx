// Source viewer drawer: page images with normalized text boxes, text blocks of
// non-paged documents, transcript segments with playback and video frames.
import React, { useEffect, useMemo, useState } from 'react';
import { api, assetUrl, seconds, type Block, type Locator, type MediaView, type PageView, type VersionPages } from './api';
import type { ViewerTarget } from './main';
import { Busy, Chip, ErrorLine, useLoad } from './ui';

export function Viewer({ target, onClose }: { target: ViewerTarget; onClose: () => void }) {
  useEffect(() => {
    const key = (e: KeyboardEvent) => e.key === 'Escape' && onClose();
    window.addEventListener('keydown', key);
    return () => window.removeEventListener('keydown', key);
  }, [onClose]);
  return (
    <aside className="drawer" aria-label="原文查看">
      <header className="drawer-head">
        <div>
          <p className="eyebrow">原文与位置</p>
          <h2 title={target.title}>{target.title}</h2>
        </div>
        <button className="icon" onClick={onClose} aria-label="关闭原文">×</button>
      </header>
      <div className="drawer-body">{target.media ? <MediaPane target={target} /> : <DocumentPane target={target} />}</div>
    </aside>
  );
}

function DocumentPane({ target }: { target: ViewerTarget }) {
  const pages = useLoad(() => api.view<VersionPages>('version', target.version), [target.version]);
  const [page, setPage] = useState<string | undefined>(target.page);
  useEffect(() => setPage(target.page), [target.page, target.version]);
  if (pages.error) return <ErrorLine text={pages.error} />;
  if (!pages.data) return <Busy />;
  const list = pages.data.pages.filter((p) => p.page_no < 100000);
  const current = page ?? list[0]?.page_id;
  return (
    <div className="stack">
      {list.length > 0 && (
        <nav className="page-tabs" aria-label="页码">
          {list.map((p) => (
            <button key={p.page_id} className={p.page_id === current ? 'on' : ''} aria-current={p.page_id === current} onClick={() => setPage(p.page_id)}>
              {p.page_no}{p.source !== 'native' && <span className="muted">·{p.source === 'scan' ? '扫描' : '图'}</span>}
            </button>
          ))}
        </nav>
      )}
      {current && <PagePane pageId={current} highlight={target.highlight?.filter((l) => !l.page_id || l.page_id === current)} />}
      {pages.data.blocks.length > 0 && <BlocksPane blocks={pages.data.blocks} highlight={target.highlight} />}
      {list.length === 0 && pages.data.blocks.length === 0 && <p className="muted">该资料没有可显示的文字。</p>}
    </div>
  );
}

function sameBox(a?: number[], b?: number[]) {
  return !!a && !!b && a.length === 4 && b.length === 4 && a.every((v, i) => Math.abs(v - b[i]) < 1e-6);
}

export function PagePane({ pageId, highlight }: { pageId: string; highlight?: Locator[] }) {
  const [view, setView] = useState<PageView | null>(null);
  const [error, setError] = useState('');
  const [active, setActive] = useState<number | null>(null);
  useEffect(() => {
    let alive = true;
    setView(null);
    setError('');
    api
      .view<PageView>('page', pageId)
      .then((v) => (v.image_token ? v : api.view<PageView>('render_page', pageId)))
      .then((v) => alive && setView(v))
      .catch((e: unknown) => alive && setError(e instanceof Error ? e.message : String(e)));
    return () => {
      alive = false;
    };
  }, [pageId]);
  const marked = useMemo(() => new Set((view?.blocks ?? []).filter((b) => highlight?.some((h) => sameBox(h.bbox, b.locator.bbox))).map((b) => b.block)), [view, highlight]);
  if (error) return <ErrorLine text={error} />;
  if (!view) return <Busy label="正在打开页面…" />;
  const shown = view.blocks.filter((b) => b.locator.bbox && b.locator.frame !== 'rectified');
  const lowConfidence = view.blocks.filter((b) => b.locator.low_confidence).length;
  return (
    <div className="page-pane">
      <div className="page-meta">
        <span>第 {view.page_no < 100000 ? view.page_no : view.page_no - 100000 + 1} {view.page_no < 100000 ? '页' : '帧'}</span>
        {view.source !== 'native' && <Chip tone="info">本机 OCR</Chip>}
        {view.rotation_turns > 0 && <Chip tone="neutral">已按方向旋转 {view.rotation_turns * 90}° 识别</Chip>}
        {view.unwarped && <Chip tone="neutral">已矫正弯曲</Chip>}
        {lowConfidence > 0 && <Chip tone="warn">{lowConfidence} 行置信度较低</Chip>}
      </div>
      {view.image_token ? (
        <div className="page-stage">
          <img src={assetUrl(view.image_token)} alt={`第 ${view.page_no} 页原图`} draggable={false} />
          {shown.map((b) => {
            const [x0, y0, x1, y1] = b.locator.bbox!;
            const on = marked.has(b.block) || active === b.block;
            return (
              <button
                key={b.block}
                className={`box${on ? ' on' : ''}${b.locator.low_confidence ? ' low' : ''}`}
                style={{ left: `${x0 * 100}%`, top: `${y0 * 100}%`, width: `${(x1 - x0) * 100}%`, height: `${(y1 - y0) * 100}%` }}
                title={b.text}
                aria-label={b.text}
                onClick={() => setActive(b.block)}
              />
            );
          })}
        </div>
      ) : (
        <p className="muted">该页没有可显示的图像。</p>
      )}
      <ol className="lines">
        {view.blocks.map((b) => (
          <li key={b.block} className={`${marked.has(b.block) || active === b.block ? 'on' : ''}${b.locator.low_confidence ? ' low' : ''}`} onClick={() => setActive(b.block)}>
            {b.text}
            {b.confidence !== null && b.locator.low_confidence && <span className="muted small">（置信度 {(b.confidence * 100).toFixed(0)}%，请对照原图）</span>}
          </li>
        ))}
      </ol>
    </div>
  );
}

function BlocksPane({ blocks, highlight }: { blocks: VersionPages['blocks']; highlight?: Locator[] }) {
  const marked = (b: VersionPages['blocks'][number]) =>
    !!highlight?.some((h) => (h.line !== undefined && h.line === b.locator.line) || (h.row !== undefined && h.row === b.locator.row && h.sheet === b.locator.sheet) || (h.paragraph !== undefined && h.paragraph === b.locator.paragraph) || (h.table !== undefined && h.table === b.locator.table && h.row === b.locator.row));
  return (
    <ol className="lines blocks">
      {blocks.map((b) => (
        <li key={b.block} className={`kind-${b.kind}${marked(b) ? ' on' : ''}`}>
          {b.text}
        </li>
      ))}
    </ol>
  );
}

function MediaPane({ target }: { target: ViewerTarget }) {
  const media = useLoad(() => api.view<MediaView>('media', target.version), [target.version]);
  const [audio, setAudio] = useState<{ segment: string; url: string } | null>(null);
  const [frame, setFrame] = useState<string | null>(null);
  if (media.error) return <ErrorLine text={media.error} />;
  if (!media.data) return <Busy />;
  const m = media.data;
  const highlighted = new Set(target.highlight?.map((l) => l.segment_id).filter(Boolean));
  if (!m.media_id) return <p className="muted">该资料没有音视频记录。</p>;
  return (
    <div className="stack">
      <p className="muted small">时长 {m.duration_s ? seconds(m.duration_s) : '未解码'} · {m.has_video ? '视频' : '音频'}{m.has_audio ? '（含音轨）' : ''}。转写为本机 ASR 文字，按 30 秒分段（相邻段重叠 1 秒），没有逐词时间戳与说话人识别。</p>
      <h3>转写片段（{m.segments?.length ?? 0}）</h3>
      {!m.segments?.length && <p className="muted">尚未转写。在资料列表中点击“转写”或“识别画面与语音”。</p>}
      <ol className="segments">
        {m.segments?.map((s) => (
          <li key={s.segment_id} className={highlighted.has(s.segment_id) ? 'on' : ''}>
            <div className="row-between">
              <span className="time">{seconds(s.start_s)}–{seconds(s.end_s)}</span>
              <button className="small" onClick={async () => { const r = await api.view<{ audio_token: string }>('segment_audio', s.segment_id); setAudio({ segment: s.segment_id, url: assetUrl(r.audio_token) }); }}>播放</button>
            </div>
            <p>{s.transcript ?? <span className="muted">（尚未转写）</span>}</p>
            {s.truncated && <Chip tone="warn">达到输出上限，可能不完整</Chip>}
            {audio?.segment === s.segment_id && <audio controls autoPlay src={audio.url} />}
          </li>
        ))}
      </ol>
      {(m.frames?.length ?? 0) > 0 && (
        <>
          <h3>抽取的画面（{m.frames!.length}）</h3>
          <p className="muted small">按视频真实时间戳均匀抽取；只识别这些画面中的文字，未抽到的时段可能有未检查的画面内容。</p>
          <div className="frames">
            {m.frames!.map((f) => (
              <button key={f.frame_id} className={frame === f.page_id ? 'on' : ''} onClick={() => setFrame(f.page_id)} disabled={!f.page_id}>
                {seconds(f.time_s)}
              </button>
            ))}
          </div>
          {frame && <PagePane pageId={frame} />}
        </>
      )}
    </div>
  );
}

export type { Block };
