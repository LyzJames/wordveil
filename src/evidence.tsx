// Evidence cards (design 8.15): number, file · location, version, confidence
// note, the exact excerpt and "定位原文".
import React from 'react';
import { seconds, type EvidenceItem, type Locator } from './api';
import { Icon } from './icons';
import { Chip } from './ui';
import { useSession } from './session';
import { REGIONS } from './regions';

export function locatorLabel(locators: Locator[]): string {
  const pages = [...new Set(locators.map((l) => l.page_no).filter((p): p is number => typeof p === 'number' && p < 100000))];
  if (pages.length) return `第 ${pages.join('、')} 页`;
  const time = locators.find((l) => l.type === 'time');
  if (time && time.start_s !== undefined) return `${seconds(time.start_s)}–${seconds(time.end_s ?? time.start_s)}`;
  if (locators.some((l) => (l.page_no ?? 0) >= 100000)) return '视频画面';
  const row = locators.find((l) => l.row !== undefined);
  if (row) return `${row.sheet ? row.sheet + ' ' : ''}第 ${row.row} 行`;
  const line = locators.find((l) => l.line !== undefined || l.paragraph !== undefined);
  if (line) return line.line !== undefined ? `第 ${line.line} 行` : `第 ${line.paragraph} 段`;
  return '';
}

export function EvidenceList({ items, onOpen, labels, current }: { items: EvidenceItem[]; onOpen: (item: EvidenceItem) => void; labels?: Record<string, string>; current?: string | null }) {
  const { open }=useSession();
  const relations:Record<string,string>={cites:'引用',amends:'修订',supersedes:'取代',evidence_for:'提供依据',confirmed_fact:'已确认事实',derived_from:'来源关系'};
  return (
    <ol className="evidence">
      {items.map((item, index) => {
        const where = locatorLabel(item.locators);
        const mediaParts = item.source_parts?.filter((p) => ['frame_ocr', 'audio_asr'].includes(p.kind)) ?? [];
        return (
          <li key={item.evidence_id} aria-current={current === item.evidence_id ? 'true' : undefined} data-evidence={item.evidence_id}>
            <div className="evidence-head">
              <span className="n">{labels?.[item.evidence_id] ?? index + 1}</span>
              <span className="t-strong ellipsis" title={item.document.display_name}>{item.document.display_name}</span>
              <span className="t-caption text-tertiary nowrap">{where ? `${where} · ` : ''}第 {item.document.version_no} 版</span>
            </div>
            {mediaParts.length ? mediaParts.map((part, i) => <div key={i} className="quote"><Chip tone="info">{part.label}</Chip><p>{part.text}</p><button className="btn btn-xs" onClick={() => { const locators = part.locators ?? [part.locator]; onOpen({ ...item, locators, highlight_indexes: locators.map((_, n) => n), pages: part.page_id ? [part.page_id] : [] }); }}>{part.kind === 'audio_asr' ? '定位音轨' : '定位画面'}</button></div>) : <div className="quote">{item.exact_text}</div>}
            {item.legal_source&&<details className="t-caption evidence-provenance"><summary>版本与地域标注</summary><dl className="kv"><dt>公布机关</dt><dd>{item.legal_source.issuer}</dd><dt>地域</dt><dd>{REGIONS.find(([id])=>id===item.legal_source?.region)?.[1]??item.legal_source.region}</dd><dt>施行日期</dt><dd>{item.legal_source.effective_on}{item.legal_source.expires_on?`；${item.legal_source.expires_on} 起失效`:''}</dd><dt>核验截至</dt><dd>{item.legal_source.verified_on}（仍需核对个案适用性）</dd></dl></details>}
            {!!item.graph_paths?.length&&<details className="t-caption evidence-provenance"><summary>找到这段原文的关系路径</summary><p>关系只用于寻找原文，不替代适用性判断和逐句核查。</p>{item.graph_paths.map((path,index)=><ol key={index}>{path.steps.map((step,i)=><li key={i}><span>{relations[step.relation_type]??'资料关系'}{step.traversal==='reverse'?'（反向查找）':''} · {step.origin==='explicit_source_reference'?'原文明示引用':'已确认的资料关系'}</span><div>{step.proof.document?.display_name}{step.proof.document?` · 第 ${step.proof.document.version_no} 版`:''}</div>{step.proof.version_id&&<button className="btn btn-xs" onClick={()=>open({version:step.proof.version_id!,title:step.proof.document?.display_name??'关系来源',page:step.proof.locators?.find((l)=>l.page_id)?.page_id,highlight:step.proof.locators})}>查看关系的原文来源</button>}</li>)}</ol>)}</details>}
            <div className="actions">
              <button className="btn btn-xs" onClick={() => onOpen(item)}>
                <Icon name="highlight" />
                定位原文
              </button>
              {item.locators.some((l) => l.low_confidence) && (
                <Chip tone="outline" title="其中有识别置信度较低的文字，金额、日期和姓名请对照原图">含低置信度文字</Chip>
              )}
              {item.document.media_kind !== 'document' && !mediaParts.length && <Chip tone="info">{item.document.media_kind === 'audio' ? '音轨转写（本机 ASR）' : '媒体文字（待核对分段来源）'}</Chip>}
            </div>
          </li>
        );
      })}
    </ol>
  );
}
