// Application shell: lock screen, navigation, live tasks and the evidence drawer.
import React, { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState } from 'react';
import { createRoot } from 'react-dom/client';
import { api, onLocked, onTask, statusLabel, type AppStatus, type Locator, type TaskState, type UnlockInfo } from './api';
import { LockScreen } from './lock';
import { MatterList, MatterPage } from './matters';
import { SearchPage } from './search';
import { SettingsPage } from './settings';
import { Busy, Chip, Notices, useNotice } from './ui';
import { Viewer } from './viewer';
import { Workbench } from './workbench';
import './style.css';

export type View =
  | { name: 'home' }
  | { name: 'matters' }
  | { name: 'matter'; id: string; tab?: string }
  | { name: 'search'; matters?: string[] }
  | { name: 'settings'; tab?: string };

export type ViewerTarget = { version: string; title: string; page?: string; highlight?: Locator[]; media?: boolean };

type Session = {
  info: UnlockInfo;
  view: View;
  go: (view: View) => void;
  open: (target: ViewerTarget) => void;
  tasks: TaskState[];
  revision: number;
  refresh: () => void;
};

const SessionContext = createContext<Session | null>(null);
export function useSession(): Session {
  const session = useContext(SessionContext);
  if (!session) throw new Error('session missing');
  return session;
}

function App() {
  const [status, setStatus] = useState<AppStatus | null>(null);
  const [info, setInfo] = useState<UnlockInfo | null>(null);
  const load = useCallback(() => api.status().then(setStatus).catch((e) => setStatus({ milestone: 'M1', version: '', installation_ok: false, error: String(e) })), []);
  useEffect(() => {
    load();
  }, [load]);
  useEffect(() => {
    const off = onLocked(() => {
      setInfo(null);
      setStatus(null);
      load();
    });
    return () => {
      off.then((f) => f());
    };
  }, [load]);
  if (!status) return <Busy label="正在检查安装…" />;
  if (!info) return <LockScreen status={status} onUnlocked={setInfo} onRefresh={load} />;
  return (
    <Workspace
      info={info}
      onLock={() => {
        // Re-read the vault list before the lock screen chooses its default form.
        setInfo(null);
        setStatus(null);
        load();
      }}
    />
  );
}

function Workspace({ info, onLock }: { info: UnlockInfo; onLock: () => void }) {
  const [view, setView] = useState<View>({ name: 'home' });
  const [viewer, setViewer] = useState<ViewerTarget | null>(null);
  const [tasks, setTasks] = useState<TaskState[]>([]);
  const [revision, setRevision] = useState(0);
  const notice = useNotice();
  const lastTouch = useRef(0);

  const refreshTasks = useCallback(() => api.tasks().then(setTasks).catch(() => {}), []);
  useEffect(() => {
    refreshTasks();
    const off = onTask((event) => {
      if (event.kind === 'finished') {
        const envelope = event.envelope;
        if (envelope) {
          const tone = envelope.status === 'completed' ? 'ok' : envelope.status === 'cancelled' ? 'info' : 'error';
          notice(tone, `任务${statusLabel(envelope.status)}${envelope.warnings.length ? '：' + envelope.warnings.map((w) => w.message).join('；') : ''}`);
        }
        setRevision((r) => r + 1);
      }
      refreshTasks();
    });
    return () => {
      off.then((f) => f());
    };
  }, [notice, refreshTasks]);

  // Activity keeps the auto-lock timer fresh (at most every 30 seconds).
  useEffect(() => {
    const ping = () => {
      const now = Date.now();
      if (now - lastTouch.current > 30000) {
        lastTouch.current = now;
        api.touch().catch(() => {});
      }
    };
    window.addEventListener('keydown', ping);
    window.addEventListener('pointerdown', ping);
    return () => {
      window.removeEventListener('keydown', ping);
      window.removeEventListener('pointerdown', ping);
    };
  }, []);

  const session = useMemo<Session>(
    () => ({ info, view, go: (v) => { setView(v); setViewer(null); }, open: setViewer, tasks, revision, refresh: () => setRevision((r) => r + 1) }),
    [info, view, tasks, revision],
  );
  const running = tasks.filter((t) => t.status === 'running');

  async function lock() {
    try {
      await api.lock();
    } finally {
      onLock();
    }
  }

  return (
    <SessionContext.Provider value={session}>
      <div className="app">
        <header className="topbar">
          <div className="topbar-brand">
            <span className="mark small" aria-hidden>律</span>
            <strong>律金智库</strong>
            <span className="vault-name" title="当前私密空间">{info.name}</span>
          </div>
          <div className="topbar-status" aria-label="能力状态">
            <Chip tone="ok" title="导入、OCR、ASR、检索、重排与计算都在本机完成">资料/检索：本机</Chip>
            <Chip tone="neutral" title="当前版本不连接生成接口；分析入口会说明需要生成接口">生成：未连接（M2）</Chip>
            <Chip tone="neutral" title="当前版本不发起任何网络请求">联网：未开放（M2）</Chip>
            {running.length > 0 && <Chip tone="info">{running.length} 个任务进行中</Chip>}
          </div>
          <button className="lock-button" onClick={lock} title="锁定后需要口令或恢复码才能再次打开">锁定</button>
        </header>
        <div className="body">
          <nav className="sidenav" aria-label="主导航">
            <NavButton current={view.name === 'home'} onClick={() => session.go({ name: 'home' })}>工作台</NavButton>
            <NavButton current={view.name === 'matters' || view.name === 'matter'} onClick={() => session.go({ name: 'matters' })}>我的事务</NavButton>
            <NavButton current={view.name === 'search'} onClick={() => session.go({ name: 'search' })}>资料检索</NavButton>
            <NavButton current={view.name === 'settings'} onClick={() => session.go({ name: 'settings' })}>设置</NavButton>
            <div className="sidenav-foot">
              <p>M1 内部版</p>
              <p>本机加密 · 断网可用</p>
            </div>
          </nav>
          <main className="content" id="main">
            {view.name === 'home' && <Workbench />}
            {view.name === 'matters' && <MatterList />}
            {view.name === 'matter' && <MatterPage id={view.id} initialTab={view.tab} />}
            {view.name === 'search' && <SearchPage initial={view.matters} />}
            {view.name === 'settings' && <SettingsPage initialTab={view.tab} onLocked={onLock} />}
          </main>
          {viewer && <Viewer target={viewer} onClose={() => setViewer(null)} />}
        </div>
      </div>
    </SessionContext.Provider>
  );
}

function NavButton({ current, onClick, children }: { current: boolean; onClick: () => void; children: React.ReactNode }) {
  return (
    <button className={current ? 'nav current' : 'nav'} aria-current={current ? 'page' : undefined} onClick={onClick}>
      {children}
    </button>
  );
}

// Light by default; dark follows the system unless the user picked one.
try {
  const theme = window.localStorage.getItem('lvjin-theme');
  if (theme === 'dark' || theme === 'light') document.documentElement.dataset.theme = theme;
} catch {
  // Storage may be unavailable; the system preference applies.
}

createRoot(document.getElementById('root')!).render(
  <Notices>
    <App />
  </Notices>,
);
