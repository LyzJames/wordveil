// Q&A thread turns shared by the Q&A page and the entries that start a
// written analysis or a media review (their results appear in the thread).
import type { Purpose, Warning } from './api';

export type TurnKind = 'answer' | 'deep' | 'evidence' | 'contract' | 'loan' | 'budget' | 'image' | 'audio' | 'video';

export type QaTurn = {
  id: string;
  question: string;
  matters: string[];
  kind: TurnKind;
  state: 'preparing' | 'running' | 'done' | 'failed' | 'cancelled' | 'interrupted';
  started: number;
  finished?: number;
  task?: string;
  status?: string;
  warnings?: Warning[];
  resultRef?: string | null;
  sent?: number;
  error?: string;
  /** Flow run of an interrupted task (continue / discard). */
  run?: string;
  /** Where an analysis or media review came from (shown instead of the scope). */
  source?: string;
};

export const KIND_LABEL: Record<TurnKind, string> = {
  answer: '生成回答', deep: '深入分析', evidence: '只找原文', contract: '合同书面分析', loan: '借款书面分析', budget: '预算书面分析',
  image: '图片分析', audio: '录音分析', video: '视频分析',
};

export const PURPOSE_KIND: Record<Purpose, TurnKind> = {
  answer: 'answer', deep_answer: 'deep', contract_review: 'contract', loan_analysis: 'loan', budget_analysis: 'budget',
  image_review: 'image', audio_review: 'audio', video_review: 'video',
};

/** Turn kind of an interrupted flow run, from its flow and owner skill. */
export function kindOfRun(flow: string, skill: string): TurnKind {
  if (flow === 'media_review') return skill === 'video-review' ? 'video' : skill === 'audio-review' ? 'audio' : 'image';
  return ({ answer: 'answer', deep_answer: 'deep', contract_review: 'contract', loan_analysis: 'loan', budget_analysis: 'budget' } as Record<string, TurnKind>)[flow] ?? 'answer';
}

/** Generated answers are written by the connection; the rest are reviews of them. */
export const generates = (kind: TurnKind) => kind !== 'evidence';

let counter = 0;
export const newTurnId = () => `turn-${Date.now().toString(36)}-${++counter}`;

export const ROLE_TITLE: Record<string, string> = {
  planner: '任务规划者', 'evidence-researcher': '证据研究者', 'legal-analyst': '法律适用分析者', 'finance-analyst': '金融口径分析者',
  'media-reader': '媒体原文读取者', 'evidence-checker': '声明证据检查者', synthesizer: '结果综合者',
};
