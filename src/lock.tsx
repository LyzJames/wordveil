// Lock screen (design 9.1): create a private space, unlock with the
// passphrase or the recovery code, or restore an encrypted backup.
import React, { useState } from 'react';
import { api, day, type AppStatus, type UnlockInfo } from './api';
import { Icon } from './icons';
import { ErrorLine, Field } from './ui';

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
        <main className="lock-card">
          <Brand />
          <h1 className="t-title">安装完整性故障</h1>
          <p className="text-secondary">{status.error}</p>
          <p>请使用同一版本的完整离线安装介质修复安装；本应用不会联网下载或修补任何组件。</p>
        </main>
      </div>
    );
  }
  if (recovery) return <RecoveryCode code={recovery.code} onDone={() => onUnlocked(recovery.info)} />;
  return (
    <div className="lock">
      <main className="lock-card fade-in">
        <Brand />
        {mode === 'unlock' && <Unlock vaults={vaults} onUnlocked={onUnlocked} />}
        {mode === 'create' && <Create onCreated={(info, code) => setRecovery({ info, code })} />}
        {mode === 'restore' && <Restore onRestored={() => { onRefresh(); setMode('unlock'); }} />}
        <nav className="lock-foot" aria-label="空间操作">
          {mode !== 'unlock' && vaults.length > 0 && <button className="btn btn-link" onClick={() => setMode('unlock')}>解锁已有空间</button>}
          {mode !== 'create' && <button className="btn btn-link" onClick={() => setMode('create')}>新建私密空间</button>}
          {mode !== 'restore' && <button className="btn btn-link" onClick={() => setMode('restore')}>从加密备份恢复</button>}
        </nav>
        <p className="lock-note">
          <Icon name="shield" />
          <span>资料只保存在这台电脑上并已加密。应用不保存口令；连续输错会逐步延长等待时间。{status.version ? `版本 ${status.version}` : ''}</span>
        </p>
      </main>
    </div>
  );
}

function Brand() {
  return (
    <div className="brand-block">
      <span className="mark lg" aria-hidden="true">律</span>
      <div>
        <div className="t-caption text-secondary">本地法律与金融工作台</div>
        <div className="t-title brand-title">律金智库</div>
      </div>
    </div>
  );
}

function Secret({ kind, value, onChange, label, autoFocus }: { kind: 'passphrase' | 'recovery'; value: string; onChange: (v: string) => void; label: string; autoFocus?: boolean }) {
  return (
    <Field label={label} hint={kind === 'recovery' ? '39 位分组字符，大小写与空格均可。' : undefined}>
      <input type={kind === 'passphrase' ? 'password' : 'text'} className={kind === 'recovery' ? 'mono' : undefined} value={value} onChange={(e) => onChange(e.target.value)} autoComplete="off" spellCheck={false} autoFocus={autoFocus} required />
    </Field>
  );
}

function Unlock({ vaults, onUnlocked }: { vaults: { handle: string; created_at: string }[]; onUnlocked: (info: UnlockInfo) => void }) {
  const [handle, setHandle] = useState(vaults[vaults.length - 1]?.handle ?? '');
  const [kind, setKind] = useState<'passphrase' | 'recovery'>('passphrase');
  const [secret, setSecret] = useState('');
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setError('');
    setBusy(true);
    try {
      const info = await api.unlock(handle, kind, secret);
      setSecret('');
      onUnlocked(info);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }
  return (
    <form onSubmit={submit} className="stack">
      <div>
        <h1 className="t-title">解锁私密空间</h1>
        <p className="text-secondary" style={{ marginTop: 4 }}>{kind === 'passphrase' ? '输入主口令。' : '忘记主口令时，用创建空间时抄写的恢复码解锁。'}</p>
      </div>
      {vaults.length > 1 && (
        <Field label="私密空间">
          <select value={handle} onChange={(e) => setHandle(e.target.value)}>
            {vaults.map((v, i) => (
              <option key={v.handle} value={v.handle}>私密空间 {i + 1} · {day(v.created_at)} 创建</option>
            ))}
          </select>
        </Field>
      )}
      <Secret key={kind} kind={kind} value={secret} onChange={setSecret} label={kind === 'passphrase' ? '主口令' : '恢复码'} autoFocus />
      <ErrorLine text={error} />
      <button className="btn btn-primary lock-submit" disabled={busy || !secret || !handle}>{busy ? '正在解锁…' : '解锁'}</button>
      <div>
        <button type="button" className="btn btn-link" onClick={() => { setKind(kind === 'passphrase' ? 'recovery' : 'passphrase'); setSecret(''); setError(''); }}>
          {kind === 'passphrase' ? '使用恢复码' : '使用主口令'}
        </button>
      </div>
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
      <div>
        <h1 className="t-title">新建私密空间</h1>
        <p className="text-secondary" style={{ marginTop: 4 }}>主口令用于解锁本机加密空间。应用不保存口令；忘记口令时只能用恢复码。</p>
      </div>
      <Field label="空间名称"><input value={name} maxLength={60} onChange={(e) => setName(e.target.value)} required /></Field>
      <Field label="主口令"><input type="password" value={pass} onChange={(e) => setPass(e.target.value)} autoComplete="new-password" required autoFocus /></Field>
      <div className="meter-row" aria-live="polite">
        <div className="meter" aria-hidden="true"><span style={{ width: `${meter.level * 25}%` }} className={`meter-${meter.level}`} /></div>
        <span className="t-caption text-tertiary">{pass ? `强度：${meter.label}` : ''}</span>
      </div>
      <Field label="再次输入主口令"><input type="password" value={again} onChange={(e) => setAgain(e.target.value)} autoComplete="new-password" required /></Field>
      <ErrorLine text={error} />
      <button className="btn btn-primary lock-submit" disabled={busy}>{busy ? '正在创建（派生密钥约需数秒）…' : '创建并生成恢复码'}</button>
    </form>
  );
}

function RecoveryCode({ code, onDone }: { code: string; onDone: () => void }) {
  const [saved, setSaved] = useState(false);
  const [copied, setCopied] = useState(false);
  return (
    <div className="lock">
      <main className="lock-card fade-in">
        <Brand />
        <div>
          <h1 className="t-title">请抄写恢复码</h1>
          <p className="text-secondary" style={{ marginTop: 4 }}>恢复码只显示这一次，应用不会保存。忘记主口令或在新电脑恢复备份时，需要它解锁。</p>
        </div>
        <div className="recovery" aria-label="恢复码">{code}</div>
        <div className="actions">
          <button className="btn btn-sm" onClick={() => navigator.clipboard.writeText(code).then(() => setCopied(true), () => setCopied(false))}>
            <Icon name="copy" />
            {copied ? '已复制' : '复制'}
          </button>
          <span className="t-caption text-tertiary">复制后请尽快粘贴到密码管理器，并清空剪贴板。</span>
        </div>
        <ul className="t-caption text-secondary plain-list">
          <li>写在纸上，或保存在你信任的密码管理器中。</li>
          <li>不要截图、拍照上传到网盘或发给他人。</li>
        </ul>
        <label className="check">
          <input type="checkbox" checked={saved} onChange={(e) => setSaved(e.target.checked)} />
          我已妥善保存恢复码
        </label>
        <button className="btn btn-primary lock-submit" disabled={!saved} onClick={onDone}>进入工作台</button>
      </main>
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
      <div>
        <h1 className="t-title">从加密备份恢复</h1>
        <p className="text-secondary" style={{ marginTop: 4 }}>先输入备份时空间的主口令或恢复码，再选择备份文件夹（LvjinBackup-…）。恢复前会校验每个文件的完整性。</p>
      </div>
      <Secret key={kind} kind={kind} value={secret} onChange={setSecret} label={kind === 'passphrase' ? '备份空间的主口令' : '恢复码'} autoFocus />
      <ErrorLine text={error} />
      {done && <p className="ok-line">{done}</p>}
      <button className="btn btn-primary lock-submit" disabled={busy || !secret}>{busy ? '正在校验并恢复…' : '选择备份文件夹并恢复'}</button>
      <div>
        <button type="button" className="btn btn-link" onClick={() => { setKind(kind === 'passphrase' ? 'recovery' : 'passphrase'); setSecret(''); }}>
          {kind === 'passphrase' ? '使用恢复码' : '使用主口令'}
        </button>
      </div>
    </form>
  );
}
