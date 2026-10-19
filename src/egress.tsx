// The in-app half of the two-step egress approval (design 9.7): the real
// parameters of one single-use grant — destination, purpose, limits, the
// excerpts and media that may leave — with items the user can leave out.
// Continuing opens the native confirmation built by the core from the same
// stored parameters; nothing is sent before that.
import React, { useState } from 'react';
import { bytes, type AnswerPreview, type Purpose } from './api';
import { locatorLabel } from './evidence';
import { Icon } from './icons';
import { Callout, Chip, Modal } from './ui';

const PURPOSE: Record<Purpose, string> = {
  answer: '生成这一次回答：改写检索词、起草，并按本机核查结果修订（不包含联网搜索）',
  deep_answer: '深入分析这一个问题：任务规划者安排至多 3 个专业角色分别分析，结果综合者起草，检查者在干净的上下文中复核，至多修订一次（不包含联网搜索）',
  contract_review: '合同书面分析：法律适用分析者与金融口径分析者分别分析，结果综合者起草，检查者复核',
  loan_analysis: '借款书面分析：金融口径分析者（依据本机确定性计算）与法律适用分析者分别分析，结果综合者起草，检查者复核',
  budget_analysis: '预算书面分析：金融口径分析者依据本机计算分析，结果综合者起草，检查者复核',
  image_review: '图片分析：先让模型直接读取下列图片（不提供本机识别文字），再与本机识别逐项核对，形成有依据的分析',
  audio_review: '录音分析：本机转写；接口通过音频输入自检时直接听取所选时段，再与转写核对',
  video_review: '视频分析：本机抽帧识别与转写；接口通过自检时直接读取抽取的画面和音轨（分开发送，不发送完整视频）',
};

export const PURPOSE_TITLE: Record<Purpose, string> = {
  answer: '生成回答', deep_answer: '深入分析', contract_review: '合同书面分析', loan_analysis: '借款书面分析', budget_analysis: '预算书面分析',
  image_review: '图片分析', audio_review: '录音分析', video_review: '视频分析',
};

function toggled(set: Set<string>, id: string): Set<string> {
  const next = new Set(set);
  if (next.has(id)) next.delete(id);
  else next.add(id);
  return next;
}

export function EgressDialog({ preview, onCancel, onConfirm }: { preview: AnswerPreview; onCancel: () => void; onConfirm: (excluded: string[], excludedMedia: string[]) => Promise<void> }) {
  const [excluded, setExcluded] = useState<Set<string>>(new Set());
  const [excludedMedia, setExcludedMedia] = useState<Set<string>>(new Set());
  const [busy, setBusy] = useState(false);
  const media = preview.native_media ?? [];
  const policy = preview.media_policy;
  const sendingText = preview.excerpts.length - excluded.size;
  const sendingMedia = media.length - excludedMedia.size;
  const mediaPurpose = preview.purpose === 'image_review' || preview.purpose === 'audio_review' || preview.purpose === 'video_review';
  // A media review may legitimately send only media, or only text produced later.
  const nothing = sendingText === 0 && sendingMedia === 0 && !mediaPurpose;
  return (
    <Modal
      title="发送前预览"
      icon="globe"
      wide
      onClose={busy ? () => {} : onCancel}
      footer={
        <>
          <span className="t-caption text-tertiary" style={{ marginRight: 'auto' }}>将发送至 {preview.connection.host}</span>
          <button className="btn" onClick={onCancel} disabled={busy}>取消</button>
          <button
            className="btn btn-primary"
            disabled={busy || (nothing && preview.excerpts.length > 0)}
            onClick={async () => {
              setBusy(true);
              await onConfirm([...excluded], [...excludedMedia]);
              setBusy(false);
            }}
          >
            {busy ? '等待系统确认…' : '继续，打开系统确认'}
          </button>
        </>
      }
    >
      <Callout title={`以下内容将发送到你配置的生成接口，用于${PURPOSE_TITLE[preview.purpose] ?? '这一次任务'}`}>可以取消勾选不想发送的内容。继续后会弹出系统确认框，确认后才会发送。</Callout>
      <dl className="kv">
        <dt>接收方</dt>
        <dd>{preview.connection.name}（{preview.connection.host}）· 模型 {preview.connection.model_id}</dd>
        <dt>用途</dt>
        <dd>{PURPOSE[preview.purpose] ?? PURPOSE.answer}</dd>
        {preview.roles && preview.roles.length > 0 && (
          <>
            <dt>参与角色</dt>
            <dd>{preview.roles.join('、')}（都由同一个生成接口完成，共用下面的请求上限）</dd>
          </>
        )}
        <dt>有效期</dt>
        <dd>仅本次任务；之后再发送需要重新确认</dd>
        <dt>上限</dt>
        <dd>最多 {preview.max_requests} 次请求；每次输出不超过 {preview.max_output_tokens.toLocaleString('zh-CN')} 个令牌；原文全程最多 {preview.max_items} 段</dd>
      </dl>
      <div>
        <div className="t-subtitle" style={{ marginBottom: 4 }}>将发送的内容</div>
        <div className="send-item">
          <Icon name="chat" />
          <div>
            <div className="t-strong">你的问题</div>
            <div className="text-secondary">{preview.question}</div>
          </div>
          <Chip>必需</Chip>
        </div>
        {media.map((m) => (
          <div key={m.media_ref} className={`send-item${excludedMedia.has(m.media_ref) ? ' excluded' : ''}`}>
            <Icon name={m.kind === 'audio' ? 'mic' : 'photo'} />
            <div style={{ minWidth: 0 }}>
              <div className="t-strong ellipsis">{[m.display, m.location].filter(Boolean).join(' · ')}</div>
              <div className="text-secondary t-caption">原始{m.kind === 'audio' ? '音频' : `图片的 JPEG 副本${m.width ? `（${m.width}×${m.height}）` : ''}`} · {bytes(m.bytes)}</div>
            </div>
            <label className="check t-caption">
              <input type="checkbox" checked={!excludedMedia.has(m.media_ref)} onChange={() => setExcludedMedia((set) => toggled(set, m.media_ref))} />
              上传
            </label>
          </div>
        ))}
        {preview.native_media_note && <p className="t-caption text-secondary">{preview.native_media_note}</p>}
        {policy && (policy.frames > 0 || policy.audio_seconds > 0) && (
          <div className="send-item">
            <Icon name={policy.frames > 0 ? 'video' : 'mic'} />
            <div>
              <div className="t-strong">运行时由本机准备的原始媒体</div>
              <div className="text-secondary">
                {[policy.frames > 0 ? `所选时段内均匀抽取的画面，最多 ${policy.frames} 张（JPEG）` : '', policy.audio_seconds > 0 ? `所选时段的原始音频，最长 ${policy.audio_seconds} 秒` : ''].filter(Boolean).join('；')}
              </div>
            </div>
            <Chip>按上限</Chip>
          </div>
        )}
        {policy && policy.frames === 0 && policy.audio_seconds === 0 && <p className="t-caption text-secondary">该连接没有通过图片或音频输入自检；只发送本机识别、转写出的文字，不上传原始媒体。</p>}
        {preview.excerpts.length === 0 && !mediaPurpose && <p className="text-secondary">所选范围内没有检索到相关原文；继续时只会发送问题，结果会说明依据不足。</p>}
        {preview.excerpts.length === 0 && mediaPurpose && preview.purpose !== 'image_review' && <p className="text-secondary">转写与抽帧识别在本机完成后，其中最多 {preview.max_items} 段文字会作为原文依据发送。</p>}
        {preview.excerpts.map((e) => (
          <div key={e.evidence_id} className={`send-item${excluded.has(e.evidence_id) ? ' excluded' : ''}`}>
            <Icon name="document" />
            <div style={{ minWidth: 0 }}>
              <div className="t-strong ellipsis">{[e.display_name, locatorLabel(e.locators ?? [])].filter(Boolean).join(' · ')}</div>
              <div className="text-secondary excerpt-text">{e.excerpt}</div>
            </div>
            <label className="check t-caption">
              <input type="checkbox" checked={!excluded.has(e.evidence_id)} onChange={() => setExcluded((set) => toggled(set, e.evidence_id))} />
              发送
            </label>
          </div>
        ))}
      </div>
      <p className="t-caption text-tertiary">
        {mediaPurpose
          ? '不会发送：原件文件本身（图片只发送重新压缩的副本）、未列出的页面、事实与金额记录、你的姓名与空间名称。'
          : '不会发送：原件与页面图、其他页面、事实与金额记录、你的姓名与空间名称。'}
        任务中可能在同一范围内换用其他段落（全程不超过 {preview.max_items} 段，取消勾选的内容不会发送）。已发送的内容无法从服务方撤回。
      </p>
    </Modal>
  );
}
