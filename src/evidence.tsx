// Evidence cards (design 8.15): number, file · location, version, confidence
// note, the exact excerpt and "定位原文".
import React from 'react';
import { seconds, type EvidenceItem, type Locator } from './api';
import { Icon } from './icons';
import { Chip } from './ui';

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
  return (
    <ol className="evidence">
      {items.map((item, index) => {
        const where = locatorLabel(item.locators);
        return (
          <li key={item.evidence_id} aria-current={current === item.evidence_id ? 'true' : undefined} data-evidence={item.evidence_id}>
            <div className="evidence-head">
              <span className="n">{labels?.[item.evidence_id] ?? index + 1}</span>
              <span className="t-strong ellipsis" title={item.document.display_name}>{item.document.display_name}</span>
              <span className="t-caption text-tertiary nowrap">{where ? `${where} · ` : ''}第 {item.document.version_no} 版</span>
            </div>
            <div className="quote">{item.exact_text}</div>
            <div className="actions">
              <button className="btn btn-xs" onClick={() => onOpen(item)}>
                <Icon name="highlight" />
                定位原文
              </button>
              {item.locators.some((l) => l.low_confidence) && (
                <Chip tone="outline" title="其中有识别置信度较低的文字，金额、日期和姓名请对照原图">含低置信度文字</Chip>
              )}
              {item.document.media_kind !== 'document' && <Chip tone="info">{item.document.media_kind === 'audio' ? '录音转写' : '视频文字'}</Chip>}
            </div>
          </li>
        );
      })}
    </ol>
  );
}
