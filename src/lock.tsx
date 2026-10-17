// Lock screen: create a private space, unlock with the passphrase or the
// recovery code, or restore an encrypted backup onto this computer.
import React, { useState } from 'react';
import { api, day, type AppStatus, type UnlockInfo } from './api';
import { ErrorLine, Field, useAction } from './ui';

function strength(text: string): { level: number; label: string } {
  let score = 0;
  if (text.length >= 8) score++;
  if (text.length >= 12) score++;
  if (/[A-Za-z]/.test(text) && /\d/.test(text)) score++;
  if (/[^A-Za-z0-9]/.test(text) || /[一-鿿]/.test(text)) score++;
  return { level: score, label: ['太短', '偏弱', '一般', '较强', '强'][score] };
}

export function LockScreen({ status, onUnlocked, onRefresh }: { status: AppStatus; onUnlocked: (info: UnlockInfo) => void; onRefresh: () => void }) {
  const vaults = status.vaults ?? [];
  const [mode, setMode] = useState<'unlock' | 'create' | 'restore'>(vaults.length ? 'unlock' : 'create');
  const [recovery, setRecovery] = useState<{ info: UnlockInfo; code: string } | null>(null);

  if (!status.installation_ok) {
    return (
      <div className="lock">
        <section className="lock-card">
          <Brand />
          <h2>安装完整性故障</h2>
          <p className="muted">{status.error}</p>
          <p>请使用同一版本的完整离线安装介质修复安装；本应用不会联网下载或修补任何组件。</p>
        </section>
      </div>
    );
  }
  if (recovery) return <RecoveryCode code={recovery.code} onDone={() => onUnlocked(recovery.info)} />;
  return (
    <div className="lock">
      <section className="lock-card">
        <Brand />
        {mode === 'unlock' && <Unlock vaults={vaults} onUnlocked={onUnlocked} />}
        {mode === 'create' && <Create onCreated={(info, code) => setRecovery({ info, code })} />}
        {mode === 'restore' && <Restore onRestored={() => { onRefresh(); setMode('unlock'); }} />}
        <nav className="lock-switch" aria-label="空间操作">
          {mode !== 'unlock' && vaults.length > 0 && <button className="link" onClick={() => setMode('unlock')}>解锁已有空间</button>}
          {mode !== 'create' && <button className="link" onClick={() => setMode('create')}>新建私密空间</button>}
          {mode !== 'restore' && <button className="link" onClick={() => setMode('restore')}>从加密备份恢复</button>}
        </nav>
        <p className="lock-foot">所有资料只保存在本机加密空间中。当前版本（{status.milestone}）不联网、不连接生成接口。</p>
      </section>
    </div>
  );
}

function Brand() {
  return (
    <header className="brand">
      <div className="mark" aria-hidden>律</div>
      <div>
        <p className="eyebrow">本地法律与金融工作台</p>
        <h1>律金智库</h1>
      </div>
    </header>
  );
}

function Unlock({ vaults, onUnlocked }: { vaults: { handle: string; created_at: string }[]; onUnlocked: (info: UnlockInfo) => void }) {
  const [handle, setHandle] = useState(vaults[vaults.length - 1]?.handle ?? '');
  const [kind, setKind] = useState<'passphrase' | 'recovery'>('passphrase');
  const [secret, setSecret] = useState('');
  const [error, setError] = useState('');
  const { busy, run } = useAction();
  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setError('');
    try {
      const info = await api.unlock(handle, kind, secret);
      setSecret('');
      onUnlocked(info);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }
  void run;
  return (
    <form onSubmit={submit} className="stack">
      <h2>解锁私密空间</h2>
      {vaults.length > 1 && (
        <Field label="空间">
          <select value={handle} onChange={(e) => setHandle(e.target.value)}>
            {vaults.map((v, i) => (
              <option key={v.handle} value={v.handle}>
                私密空间 {i + 1}（创建于 {day(v.created_at)}）
              </option>
            ))}
          </select>
        </Field>
      )}
      <div className="segmented" role="radiogroup" aria-label="解锁方式">
        <button type="button" role="radio" aria-checked={kind === 'passphrase'} className={kind === 'passphrase' ? 'on' : ''} onClick={() => setKind('passphrase')}>主口令</button>
        <button type="button" role="radio" aria-checked={kind === 'recovery'} className={kind === 'recovery' ? 'on' : ''} onClick={() => setKind('recovery')}>恢复码</button>
      </div>
      <Field label={kind === 'passphrase' ? '主口令' : '恢复码'} hint={kind === 'recovery' ? '39 位分组字符，大小写与空格均可。' : undefined}>
        <input type={kind === 'passphrase' ? 'password' : 'text'} value={secret} onChange={(e) => setSecret(e.target.value)} autoComplete="off" spellCheck={false} autoFocus required />
      </Field>
      <ErrorLine text={error} />
      <button className="primary" disabled={busy || !secret || !handle}>{busy ? '正在解锁…' : '解锁'}</button>
    </form>
  );
}

function Create({ onCreated }: { onCreated: (info: UnlockInfo, code: string) => void }) {
  const [name, setName] = useState('我的私密空间');
  const [pass, setPass] = useState('');
  const [again, setAgain] = useState('');
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  const meter = strength(pass);
  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (pass !== again) return setError('两次输入的口令不一致。');
    if ([...pass].length < 8) return setError('主口令至少 8 个字符；建议 12 个以上并混合字母、数字或中文。');
    setBusy(true);
    setError('');
    try {
      const created = await api.createVault(name.trim(), pass);
      setPass('');
      setAgain('');
      onCreated(created.info, created.recovery_code);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }
  return (
    <form onSubmit={submit} className="stack">
      <h2>新建私密空间</h2>
      <p className="muted">主口令用于解锁本机加密空间。应用不保存口令；忘记口令时只能用恢复码。</p>
      <Field label="空间名称"><input value={name} maxLength={60} onChange={(e) => setName(e.target.value)} required /></Field>
      <Field label="主口令"><input type="password" value={pass} onChange={(e) => setPass(e.target.value)} autoComplete="new-password" required /></Field>
      <div className="meter" aria-label={`口令强度：${meter.label}`}>
        <span style={{ width: `${meter.level * 25}%` }} className={`meter-${meter.level}`} />
        <em>{pass ? meter.label : ''}</em>
      </div>
      <Field label="再次输入主口令"><input type="password" value={again} onChange={(e) => setAgain(e.target.value)} autoComplete="new-password" required /></Field>
      <ErrorLine text={error} />
      <button className="primary" disabled={busy}>{busy ? '正在创建（派生密钥约需数秒）…' : '创建并生成恢复码'}</button>
    </form>
  );
}

function RecoveryCode({ code, onDone }: { code: string; onDone: () => void }) {
  const [copied, setCopied] = useState(false);
  return (
    <div className="lock">
      <section className="lock-card">
        <Brand />
        <h2>请抄写恢复码</h2>
        <p>恢复码只显示这一次，应用不会保存。忘记主口令或在新电脑恢复备份时，需要它解锁。</p>
        <div className="recovery" aria-label="恢复码">{code}</div>
        <ul className="muted small">
          <li>抄写在纸上或保存在你信任的密码管理器中。</li>
          <li>不要拍照上传到网盘或发给他人。</li>
        </ul>
        <label className="check">
          <input type="checkbox" checked={copied} onChange={(e) => setCopied(e.target.checked)} />
          我已妥善保存恢复码
        </label>
        <button className="primary" disabled={!copied} onClick={onDone}>进入工作台</button>
      </section>
    </div>
  );
}

function Restore({ onRestored }: { onRestored: () => void }) {
  const [kind, setKind] = useState<'passphrase' | 'recovery'>('passphrase');
  const [secret, setSecret] = useState('');
  const [error, setError] = useState('');
  const [done, setDone] = useState('');
  const [busy, setBusy] = useState(false);
  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError('');
    try {
      const restored = await api.restore(kind, secret);
      if (restored) {
        setSecret('');
        setDone('已恢复到本机新的空间目录，现有空间未被覆盖。请用备份时的口令或恢复码解锁。');
        onRestored();
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }
  return (
    <form onSubmit={submit} className="stack">
      <h2>从加密备份恢复</h2>
      <p className="muted">先输入备份时空间的主口令或恢复码，再选择备份文件夹（LvjinBackup-…）。恢复前会校验每个文件的完整性。</p>
      <div className="segmented" role="radiogroup" aria-label="解锁方式">
        <button type="button" role="radio" aria-checked={kind === 'passphrase'} className={kind === 'passphrase' ? 'on' : ''} onClick={() => setKind('passphrase')}>主口令</button>
        <button type="button" role="radio" aria-checked={kind === 'recovery'} className={kind === 'recovery' ? 'on' : ''} onClick={() => setKind('recovery')}>恢复码</button>
      </div>
      <Field label={kind === 'passphrase' ? '备份空间的主口令' : '恢复码'}>
        <input type={kind === 'passphrase' ? 'password' : 'text'} value={secret} onChange={(e) => setSecret(e.target.value)} autoComplete="off" required />
      </Field>
      <ErrorLine text={error} />
      {done && <p className="ok-line">{done}</p>}
      <button className="primary" disabled={busy || !secret}>{busy ? '正在校验并恢复…' : '选择备份文件夹并恢复'}</button>
    </form>
  );
}
