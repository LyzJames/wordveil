// Inspector (design 8.11, 9.4): page images with normalized text boxes and
// the cited ones highlighted, page navigation, zoom and box toggle; text
// blocks of non-paged documents; transcript segments and video frames; and
// the evidence list of an answer with the selected source below it.
import React, { useEffect, useMemo, useRef, useState } from 'react';
import { api, assetUrl, seconds, type Locator, type MediaView, type PageView, type VersionPages } from './api';
import { EvidenceList } from './evidence';
import { Icon } from './icons';
import { sourceOf, useSession, type EvidencePanel, type InspectorTarget, type ViewerTarget } from './session';
import { preference, savePreference } from './settings';
import { Busy, Chip, ErrorLine, Segmented, useLoad } from './ui';

const MIN_W = 360;
const MAX_W = 760;

function typing(target: EventTarget | null) {
  const el = target as HTMLElement | null;
  return !!el && (el.isContentEditable || ['INPUT', 'TEXTAREA', 'SELECT'].includes(el.tagName));
}

export function Inspector({ target, onClose, overlay }: { target: InspectorTarget; onClose: () => void; overlay: boolean }) {
  const ref = useRef<HTMLElement>(null);
  const [width, setWidth] = useState(() => Number(preference('lvjin-inspector-w')) || 0);
  const widthRef = useRef(width);
  widthRef.current = width;
  const close = useRef(onClose);
  close.current = onClose;

  // Escape closes the inspector unless a dialog or the page used the key.
  useEffect(() => {
    const key = (e: KeyboardEvent) => {
      if (e.key === 'Escape' && !e.defaultPrevented && !document.querySelector('.overlay')) close.current();
    };
    window.addEventListener('keydown', key);
    return () => window.removeEventListener('keydown', key);
  }, []);

  function clamp(value: number) {
    return Math.min(MAX_W, Math.max(MIN_W, Math.round(value)));
  }
  function startResize(e: React.PointerEvent) {
    e.preventDefault();
    const right = ref.current!.getBoundingClientRect().right;
    const move = (ev: PointerEvent) => setWidth(clamp(right - ev.clientX));
    const up = () => {
      window.removeEventListener('pointermove', move);
      window.removeEventListener('pointerup', up);
      savePreference('lvjin-inspector-w', String(widthRef.current));
    };
    window.addEventListener('pointermove', move);
    window.addEventListener('pointerup', up);
  }
  function keyResize(e: React.KeyboardEvent) {
    if (e.key !== 'ArrowLeft' && e.key !== 'ArrowRight') return;
    e.preventDefault();
    const next = clamp((width || ref.current!.getBoundingClientRect().width) + (e.key === 'ArrowLeft' ? 16 : -16));
    setWidth(next);
    savePreference('lvjin-inspector-w', String(next));
  }

  return (
    <aside className="inspector fade-in" aria-label={target.kind === 'evidence' ? '依据' : '原文查看'} ref={ref} style={!overlay && width ? { width } : undefined}>
      {!overlay && (
        <div className="inspector-resize" role="separator" aria-orientation="vertical" aria-label="调整面板宽度" aria-valuemin={MIN_W} aria-valuemax={MAX_W} aria-valuenow={width || undefined} tabIndex={0} onPointerDown={startResize} onKeyDown={keyResize} />
      )}
      {target.kind === 'evidence' ? <EvidenceInspector panel={target.evidence} source={target.source} onClose={onClose} /> : <SourceInspector source={target.source} onClose={onClose} />}
    </aside>
  );
}

function Head({ icon, title, subtitle, onClose, closeLabel }: { icon: 'document' | 'mic'; title: string; subtitle: string; onClose: () => void; closeLabel: string }) {
  return (
    <div className="inspector-head">
      <Icon name={icon} size="lg" />
      <div className="titles">
        <div className="t-strong ellipsis" title={title}>{title}</div>
        <div className="t-caption text-tertiary ellipsis">{subtitle}</div>
      </div>
      <button className="btn btn-subtle btn-icon btn-sm" aria-label={closeLabel} title={`${closeLabel}（Esc）`} onClick={onClose}>
        <Icon name="close" />
      </button>
    </div>
  );
}

function SourceInspector({ source, onClose }: { source: ViewerTarget; onClose: () => void }) {
  return (
    <>
      <Head icon={source.media ? 'mic' : 'document'} title={source.title} subtitle={source.media ? '录音与视频：转写分段与画面' : '原文与位置'} onClose={onClose} closeLabel="关闭原文" />
      {source.media ? (
        <div className="inspector-body">
          <MediaPane target={source} />
        </div>
      ) : (
        <DocumentPane key={source.version} target={source} />
      )}
    </>
  );
}

function EvidenceInspector({ panel, source, onClose }: { panel: EvidencePanel; source?: ViewerTarget; onClose: () => void }) {
  const { inspect } = useSession();
  const body = useRef<HTMLDivElement>(null);
  // Opening a citation brings its highlighted page into view; the cited
  // card stays marked in the list above it.
  useEffect(() => {
    const target = source ? body.current?.querySelector('.evidence-source') : panel.current ? body.current?.querySelector(`[data-evidence="${panel.current}"]`) : null;
    target?.scrollIntoView({ block: source ? 'start' : 'nearest' });
  }, [panel.current, source?.version, source?.page]);
  return (
    <>
      <Head icon="document" title={panel.title} subtitle={panel.note ?? '本次结果使用的原文'} onClose={onClose} closeLabel="关闭依据" />
      <div className="inspector-body" ref={body}>
        <EvidenceList items={panel.items} labels={panel.labels} current={panel.current} onOpen={(item) => inspect({ kind: 'evidence', evidence: { ...panel, current: item.evidence_id }, source: sourceOf(item) })} />
        {source && (
          <div className="evidence-source">
            <div className="row-between">
              <span className="t-strong ellipsis">{source.title}</span>
              <button className="btn btn-xs btn-subtle" onClick={() => inspect({ kind: 'source', source })}>
                <Icon name="openPane" />
                单独查看
              </button>
            </div>
            {source.media ? <MediaPane target={source} /> : <DocumentPane key={`${source.version}-${source.page}`} target={source} embedded />}
          </div>
        )}
      </div>
    </>
  );
}

// ---------------------------------------------------------------- documents

function DocumentPane({ target, embedded }: { target: ViewerTarget; embedded?: boolean }) {
  const pages = useLoad(() => api.view<VersionPages>('version', target.version), [target.version]);
  const [page, setPage] = useState<string | undefined>(target.page);
  const [zoom, setZoom] = useState<'fit' | 'actual'>('fit');
  const [boxes, setBoxes] = useState(true);
  const list = useMemo(() => pages.data?.pages.filter((p) => p.page_no < 100000) ?? [], [pages.data]);
  const index = Math.max(0, list.findIndex((p) => p.page_id === (page ?? list[0]?.page_id)));
  const current = list[index]?.page_id;
  const [text, setText] = useState('');
  useEffect(() => setText(String(index + 1)), [index]);

  // [ and ] turn pages (design §11).
  const turn = useRef<(delta: number) => void>(() => {});
  turn.current = (delta: number) => {
    const next = list[index + delta];
    if (next) setPage(next.page_id);
  };
  useEffect(() => {
    if (embedded) return;
    const key = (e: KeyboardEvent) => {
      if (e.ctrlKey || e.altKey || typing(e.target) || document.querySelector('.overlay')) return;
      if (e.key === '[') turn.current(-1);
      else if (e.key === ']') turn.current(1);
    };
    window.addEventListener('keydown', key);
    return () => window.removeEventListener('keydown', key);
  }, [embedded]);

  if (pages.error) return <div className="inspector-body"><ErrorLine text={pages.error} /></div>;
  if (!pages.data) return <div className="inspector-body"><Busy label="正在打开资料…" /></div>;
  const source = list[index]?.source;
  const toolbar = list.length > 0 && (
    <div className={embedded ? 'pane-toolbar' : 'inspector-toolbar'}>
      <div className="pager">
        <button className="btn btn-subtle btn-icon btn-sm" aria-label="上一页" title="上一页（[）" disabled={index === 0} onClick={() => turn.current(-1)}>
          <Icon name="chevronLeft" />
        </button>
        <input
          className="input"
          aria-label="页码"
          value={text}
          inputMode="numeric"
          onChange={(e) => setText(e.target.value.replace(/\D/g, ''))}
          onKeyDown={(e) => {
            if (e.key === 'Enter') {
              const n = Number(text);
              if (n >= 1 && n <= list.length) setPage(list[n - 1].page_id);
              else setText(String(index + 1));
            }
          }}
          onBlur={() => setText(String(index + 1))}
        />
        <span className="text-secondary">/ {list.length}</span>
        <button className="btn btn-subtle btn-icon btn-sm" aria-label="下一页" title="下一页（]）" disabled={index >= list.length - 1} onClick={() => turn.current(1)}>
          <Icon name="chevronRight" />
        </button>
      </div>
      <Segmented label="缩放" value={zoom} onChange={setZoom} items={[['fit', '适合宽度'], ['actual', '100%']]} />
      <label className="check t-caption">
        <input type="checkbox" checked={boxes} onChange={(e) => setBoxes(e.target.checked)} />
        显示文字框
      </label>
      {source && source !== 'native' && <Chip tone="info">{source === 'scan' ? '扫描页 · 本机 OCR' : '图片 · 本机 OCR'}</Chip>}
      {target.highlight?.length ? (
        <span className="chip attention located">
          <Icon name="highlight" />
          已定位
        </span>
      ) : null}
    </div>
  );
  const content = (
    <>
      {current && <PagePane pageId={current} highlight={target.highlight?.filter((l) => !l.page_id || l.page_id === current)} zoom={zoom} boxes={boxes} />}
      {pages.data.blocks.length > 0 && <BlocksPane blocks={pages.data.blocks} highlight={target.highlight} />}
      {list.length === 0 && pages.data.blocks.length === 0 && <p className="text-secondary">该资料没有可显示的文字。</p>}
    </>
  );
  return embedded ? (
    <div className="stack">
      {toolbar}
      {content}
    </div>
  ) : (
    <>
      {toolbar}
      <div className="inspector-body">{content}</div>
    </>
  );
}

function sameBox(a?: number[], b?: number[]) {
  return !!a && !!b && a.length === 4 && b.length === 4 && a.every((v, i) => Math.abs(v - b[i]) < 1e-6);
}

export function PagePane({ pageId, highlight, zoom = 'fit', boxes = true }: { pageId: string; highlight?: Locator[]; zoom?: 'fit' | 'actual'; boxes?: boolean }) {
  const [view, setView] = useState<PageView | null>(null);
  const [error, setError] = useState('');
  const [active, setActive] = useState<number | null>(null);
  const stage = useRef<HTMLDivElement>(null);
  useEffect(() => {
    let alive = true;
    setView(null);
    setError('');
    setActive(null);
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
  const frame = view.page_no >= 100000;
  return (
    <div className="page-pane">
      <div className="page-meta">
        <span className="t-caption text-secondary">第 {frame ? view.page_no - 100000 + 1 : view.page_no} {frame ? '帧' : '页'}</span>
        {view.rotation_turns > 0 && <Chip>已按方向旋转 {view.rotation_turns * 90}° 识别</Chip>}
        {view.unwarped && <Chip>已矫正弯曲</Chip>}
        {lowConfidence > 0 && <Chip tone="outline">{lowConfidence} 行置信度较低</Chip>}
      </div>
      {view.image_token ? (
        <div className={`page-scroll${zoom === 'actual' ? ' actual' : ''}`}>
          <div className={`page-stage${zoom === 'actual' ? ' zoom-actual' : ''}${boxes ? '' : ' hide-boxes'}`} ref={stage}>
            <img
              src={assetUrl(view.image_token)}
              alt={`第 ${view.page_no} 页原图`}
              draggable={false}
              onLoad={() => stage.current?.querySelector('.box.on')?.scrollIntoView({ block: 'center', behavior: 'smooth' })}
            />
            {shown.map((b) => {
              const [x0, y0, x1, y1] = b.locator.bbox!;
              const on = marked.has(b.block) || active === b.block;
              return (
                <button
                  key={b.block}
                  className={`box${on ? ' on' : ''}${marked.has(b.block) ? ' flash' : ''}${b.locator.low_confidence ? ' low' : ''}`}
                  style={{ left: `${x0 * 100}%`, top: `${y0 * 100}%`, width: `${(x1 - x0) * 100}%`, height: `${(y1 - y0) * 100}%` }}
                  title={b.text}
                  aria-label={b.text}
                  onClick={() => setActive(b.block)}
                />
              );
            })}
          </div>
        </div>
      ) : (
        <p className="text-secondary">该页没有可显示的图像。</p>
      )}
      <div className="inspector-text card">
        <div className="t-caption text-secondary" style={{ marginBottom: 6 }}>本页文字（点击行可定位）</div>
        <ol className="lines">
          {view.blocks.map((b) => (
            <li key={b.block} className={`${marked.has(b.block) || active === b.block ? 'on' : ''}${b.locator.low_confidence ? ' low' : ''}`} onClick={() => setActive(b.block)}>
              {b.text}
              {b.confidence !== null && b.locator.low_confidence && <span className="t-caption text-tertiary">（置信度 {(b.confidence * 100).toFixed(0)}%，请对照原图）</span>}
            </li>
          ))}
        </ol>
      </div>
    </div>
  );
}

function BlocksPane({ blocks, highlight }: { blocks: VersionPages['blocks']; highlight?: Locator[] }) {
  const marked = (b: VersionPages['blocks'][number]) =>
    !!highlight?.some((h) => (h.line !== undefined && h.line === b.locator.line) || (h.row !== undefined && h.row === b.locator.row && h.sheet === b.locator.sheet) || (h.paragraph !== undefined && h.paragraph === b.locator.paragraph) || (h.table !== undefined && h.table === b.locator.table && h.row === b.locator.row));
  const list = useRef<HTMLOListElement>(null);
  useEffect(() => {
    list.current?.querySelector('li.on')?.scrollIntoView({ block: 'center' });
  }, [blocks]);
  return (
    <ol className="lines blocks card" ref={list}>
      {blocks.map((b) => (
        <li key={b.block} className={`kind-${b.kind}${marked(b) ? ' on' : ''}`}>
          {b.text}
        </li>
      ))}
    </ol>
  );
}

// -------------------------------------------------------------------- media

function MediaPane({ target }: { target: ViewerTarget }) {
  const media = useLoad(() => api.view<MediaView>('media', target.version), [target.version]);
  const [audio, setAudio] = useState<{ segment: string; url: string } | null>(null);
  const [frame, setFrame] = useState<string | null>(null);
  const list = useRef<HTMLOListElement>(null);
  useEffect(() => {
    list.current?.querySelector('li.on')?.scrollIntoView({ block: 'nearest' });
  }, [media.data]);
  if (media.error) return <ErrorLine text={media.error} />;
  if (!media.data) return <Busy />;
  const m = media.data;
  const highlighted = new Set(target.highlight?.map((l) => l.segment_id).filter(Boolean));
  if (!m.media_id) return <p className="text-secondary">该资料没有音视频记录。</p>;
  return (
    <div className="stack">
      <p className="t-caption text-secondary">
        时长 {m.duration_s ? seconds(m.duration_s) : '未解码'} · {m.has_video ? '视频' : '音频'}
        {m.has_audio ? '（含音轨）' : ''}。转写为本机 ASR 文字，按 30 秒分段（相邻段重叠 1 秒），没有逐词时间戳与说话人识别。
      </p>
      <h3 className="t-subtitle">转写片段（{m.segments?.length ?? 0}）</h3>
      {!m.segments?.length && <p className="text-secondary">尚未转写。在资料列表中点击“转写”或“识别画面与语音”。</p>}
      <ol className="segments" ref={list}>
        {m.segments?.map((s) => (
          <li key={s.segment_id} className={highlighted.has(s.segment_id) ? 'on' : ''}>
            <div className="row-between">
              <span className="time">{seconds(s.start_s)}–{seconds(s.end_s)}</span>
              <button className="btn btn-xs" onClick={async () => { const r = await api.view<{ audio_token: string }>('segment_audio', s.segment_id); setAudio({ segment: s.segment_id, url: assetUrl(r.audio_token) }); }}>
                <Icon name="play" />
                播放
              </button>
            </div>
            <p>{s.transcript ?? <span className="text-tertiary">（尚未转写）</span>}</p>
            {s.truncated && <Chip tone="warning">达到输出上限，可能不完整</Chip>}
            {audio?.segment === s.segment_id && <audio controls autoPlay src={audio.url} />}
          </li>
        ))}
      </ol>
      {(m.frames?.length ?? 0) > 0 && (
        <>
          <h3 className="t-subtitle">抽取的画面（{m.frames!.length}）</h3>
          <p className="t-caption text-tertiary">按视频真实时间戳均匀抽取；只识别这些画面中的文字，未抽到的时段可能有未检查的画面内容。</p>
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
