// Windows 11 system icon font (Segoe Fluent Icons). Code points verified
// against the installed SegoeIcons.ttf; nothing is bundled.
import React from 'react';

const GLYPHS = {
  home: 0xe80f, folder: 0xe8b7, chat: 0xe8f2, search: 0xe721, library: 0xe8f1, robot: 0xe99a, connect: 0xe703, settings: 0xe713,
  lock: 0xe72e, shield: 0xea18, globe: 0xe774, permissions: 0xe8d7, import: 0xe8b5, share: 0xe72d, document: 0xe8a5, photo: 0xe91b,
  mic: 0xe720, video: 0xe714, calculator: 0xe8ef, calendar: 0xe787, link: 0xe71b, highlight: 0xe7e6, check: 0xe73e, info: 0xe946,
  warning: 0xe7ba, error: 0xea39, stop: 0xe71a, send: 0xe724, more: 0xe712, close: 0xe711, add: 0xe710, edit: 0xe70f, delete: 0xe74d,
  refresh: 0xe72c, nav: 0xe700, openPane: 0xe8a0, closePane: 0xe89f, history: 0xe81c, filter: 0xe71c, chevronDown: 0xe70d,
  chevronRight: 0xe76c, chevronLeft: 0xe76b, copy: 0xe8c8, flag: 0xe7c1, list: 0xe8fd, pin: 0xe718, unknown: 0xe9ce, clock: 0xe917,
  zoomIn: 0xe8a3, zoomOut: 0xe71f, view: 0xe890, play: 0xe768, color: 0xe790, font: 0xe8d2,
} as const;

export type IconName = keyof typeof GLYPHS;

export function Icon({ name, size, className }: { name: IconName; size?: 'lg' | 'xl'; className?: string }) {
  return (
    <span className={['icon', size, className].filter(Boolean).join(' ')} aria-hidden="true">
      {String.fromCodePoint(GLYPHS[name])}
    </span>
  );
}
