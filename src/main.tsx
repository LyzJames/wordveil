// Application shell (design §5–§6): lock screen; app bar with privacy and
// connection status, task center, command palette entry and lock; collapsible
// sidebar; the content stage with the inspector beside or over it.
import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { createRoot } from 'react-dom/client';
import { api, onLocked, onTask, relative, skillName, snapshot, stageName, statusLabel, type AppStatus, type Connection, type DueTodo, type Matter, type TaskState, type UnlockInfo } from './api';
import { ConnectionsPage } from './connections';
import { Icon, type IconName } from './icons';
import { LockScreen } from './lock';
import { MatterList, MatterPage } from './matters';
import { QaPage, watchQaTasks, type QaTurn } from './qa';
import { SessionContext, useSession, type InspectorTarget, type Session, type View } from './session';
import { applyPreferences, isLockKey, lockKeyLabel, preference, savePreference, SettingsPage, usePreferences } from './settings';
import { Busy, Empty, Notices, useDismiss, useFocusTrap, useNotice } from './ui';
import { Inspector } from './viewer';
import { NewMatter, RetryButton, Workbench } from './workbench';
import './tokens.css';
import './style.css';
import './app.css';

function App() {
  const [status, setStatus] = useState<AppStatus | null>(null);
  const [info, setInfo] = useState<UnlockInfo | null>(null);
  const load = useCallback(() => api.status().then(setStatus).catch((e) => setStatus({ milestone: 'M2', version: '', installation_ok: false, error: String(e) })), []);
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

function useWindowWidth() {
  const [width, setWidth] = useState(window.innerWidth);
  useEffect(() => {
    const resize = () => setWidth(window.innerWidth);
    window.addEventListener('resize', resize);
    return () => window.removeEventListener('resize', resize);
  }, []);
  return width;
}

function Workspace({ info, onLock }: { info: UnlockInfo; onLock: () => void }) {
  const [view, setView] = useState<View>({ name: 'home' });
  const [inspector, setInspector] = useState<InspectorTarget | null>(null);
  const [tasks, setTasks] = useState<TaskState[]>([]);
  const [revision, setRevision] = useState(0);
  const [matters, setMatters] = useState<Matter[] | null>(null);
  const [due, setDue] = useState<DueTodo[]>([]);
  const [connections, setConnections] = useState<Connection[]>([]);
  const [connectionTick, setConnectionTick] = useState(0);
  const [turns, setTurns] = useState<QaTurn[]>([]);
  const [palette, setPalette] = useState(false);
  const [creating, setCreating] = useState(false);
  const [pinnedCollapsed, setPinnedCollapsed] = useState(() => preference('lvjin-sidebar') === 'collapsed');
  const [expandedOnce, setExpandedOnce] = useState(false);
  const width = useWindowWidth();
  const notice = useNotice();
  const lastTouch = useRef(0);

  const refreshTasks = useCallback(() => api.tasks().then(setTasks).catch(() => {}), []);
  useEffect(() => {
    refreshTasks();
    const off = onTask((event) => {
      if (event.kind === 'finished') {
        const envelope = event.envelope;
        // Answers report in their own card; other background work gets a toast.
        if (envelope && envelope.skill_id !== 'lvjin-retrieval') {
          const tone = envelope.status === 'completed' ? 'ok' : envelope.status === 'cancelled' ? 'info' : 'error';
          notice(tone, `${skillName(envelope.skill_id)}${statusLabel(envelope.status)}${envelope.warnings.length ? '：' + envelope.warnings.map((w) => w.message).join('；') : ''}`);
        }
        setRevision((r) => r + 1);
      }
      refreshTasks();
    });
    return () => {
      off.then((f) => f());
    };
  }, [notice, refreshTasks]);

  useEffect(() => {
    snapshot(info.root_matter)
      .then((s) => {
        setMatters(s.matters ?? []);
        setDue(s.due_todos ?? []);
      })
      .catch(() => setMatters((m) => m ?? []));
  }, [info.root_matter, revision]);
  useEffect(() => {
    api.connections().then(setConnections).catch(() => setConnections([]));
  }, [connectionTick]);
  useEffect(() => watchQaTasks(tasks, setTurns), [tasks, turns]);

  // Activity keeps the auto-lock timer fresh (at most every 30 seconds):
  // typing, clicking, scrolling and moving the pointer over the window.
  useEffect(() => {
    const ping = () => {
      const now = Date.now();
      if (now - lastTouch.current > 30000) {
        lastTouch.current = now;
        api.touch().catch(() => {});
      }
    };
    const events = ['keydown', 'pointerdown', 'pointermove', 'wheel'] as const;
    for (const name of events) window.addEventListener(name, ping, { passive: true });
    return () => {
      for (const name of events) window.removeEventListener(name, ping);
    };
  }, []);

  const lock = useCallback(async () => {
    try {
      await api.lock();
    } finally {
      onLock();
    }
  }, [onLock]);

  const go = useCallback((next: View) => {
    setView(next);
    setInspector(null);
    document.getElementById('main')?.scrollTo?.(0, 0);
  }, []);

  // Global shortcuts (design §11).
  useEffect(() => {
    const key = (e: KeyboardEvent) => {
      const ctrl = e.ctrlKey && !e.altKey && !e.shiftKey;
      // Lock always works; other shortcuts wait until open dialogs close.
      const blocked = !!document.querySelector('.overlay');
      if (isLockKey(e)) {
        e.preventDefault();
        lock();
      } else if (blocked) {
        return;
      } else if (ctrl && e.key.toLowerCase() === 'k') {
        e.preventDefault();
        setPalette(true);
      } else if (ctrl && e.key.toLowerCase() === 'n') {
        e.preventDefault();
        setCreating(true);
      } else if (ctrl && e.key.toLowerCase() === 'f') {
        e.preventDefault();
        go({ name: 'qa', matters: view.name === 'matter' ? [view.id] : undefined });
      } else if (e.key === 'F6') {
        e.preventDefault();
        const regions = ['.sidebar', '#main', '.inspector'].map((s) => document.querySelector<HTMLElement>(s)).filter((x): x is HTMLElement => !!x);
        const index = regions.findIndex((r) => r.contains(document.activeElement));
        const next = regions[(index + (e.shiftKey ? regions.length - 1 : 1)) % regions.length];
        (next.querySelector<HTMLElement>('button,[href],input,textarea,select,[tabindex]') ?? next).focus();
      }
    };
    window.addEventListener('keydown', key);
    return () => window.removeEventListener('keydown', key);
  }, [lock, go, view]);

  const connection = connections.find((c) => c.is_default) ?? connections[0] ?? null;
  const session = useMemo<Session>(
    () => ({
      info, view, go, inspector, tasks, revision, matters: matters ?? [], mattersLoaded: matters !== null, due, connections, connection,
      open: (source) => setInspector({ kind: 'source', source }),
      inspect: setInspector,
      refresh: () => setRevision((r) => r + 1),
      reloadConnections: () => setConnectionTick((t) => t + 1),
      newMatter: () => setCreating(true),
      lock,
      qa: { turns, update: setTurns },
    }),
    [info, view, go, inspector, tasks, revision, matters, due, connections, connection, lock, turns],
  );

  useEffect(() => setExpandedOnce(false), [inspector === null]);
  const auto = width < 1280 || (inspector !== null && width < 1440);
  const collapsed = pinnedCollapsed || (auto && !expandedOnce);
  function toggleSidebar() {
    if (collapsed) {
      if (pinnedCollapsed) {
        setPinnedCollapsed(false);
        savePreference('lvjin-sidebar', 'expanded');
      }
      if (auto) setExpandedOnce(true);
    } else {
      setPinnedCollapsed(true);
      savePreference('lvjin-sidebar', 'collapsed');
    }
  }

  return (
    <SessionContext.Provider value={session}>
      <div className="app">
        <AppBar collapsed={collapsed} onToggle={toggleSidebar} onPalette={() => setPalette(true)} />
        <div className={`frame${collapsed ? ' collapsed' : ''}`}>
          <Sidebar collapsed={collapsed} />
          <div className="stage">
            <main className="content" id="main" tabIndex={-1}>
              {view.name === 'home' && <Workbench />}
              {view.name === 'matters' && <MatterList />}
              {view.name === 'matter' && <MatterPage key={view.id} id={view.id} initialTab={view.tab} />}
              {view.name === 'qa' && <QaPage key={`${(view.matters ?? []).join(',')}|${view.question ?? ''}`} initialMatters={view.matters} initialQuestion={view.question} />}
              {view.name === 'connections' && <ConnectionsPage />}
              {view.name === 'knowledge' && <Later page="knowledge" />}
              {view.name === 'tools' && <Later page="tools" />}
              {view.name === 'settings' && <SettingsPage key={view.tab ?? ''} initialTab={view.tab} />}
            </main>
            {inspector && <Inspector target={inspector} onClose={() => setInspector(null)} overlay={width < 1280} />}
          </div>
        </div>
      </div>
      {palette && <CommandPalette onClose={() => setPalette(false)} />}
      {creating && (
        <NewMatter
          root={info.root_matter}
          onClose={() => setCreating(false)}
          onCreated={(id) => {
            setCreating(false);
            setRevision((r) => r + 1);
            go({ name: 'matter', id });
          }}
        />
      )}
    </SessionContext.Provider>
  );
}

// ------------------------------------------------------------------ app bar

function AppBar({ collapsed, onToggle, onPalette }: { collapsed: boolean; onToggle: () => void; onPalette: () => void }) {
  const { info, tasks, connection, lock } = useSession();
  const [panel, setPanel] = useState<'status' | 'tasks' | null>(null);
  usePreferences();
  const lockHint = lockKeyLabel();
  const running = tasks.filter((t) => t.status === 'running');
  const fractions = running.map((t) => (t.progress && t.progress.total > 0 ? t.progress.done / t.progress.total : 0));
  const progress = fractions.length ? Math.round((fractions.reduce((a, b) => a + b, 0) / fractions.length) * 100) : 0;
  return (
    <header className="appbar">
      <button className="btn btn-subtle btn-icon" aria-label={collapsed ? '展开侧栏' : '收起侧栏'} title={collapsed ? '展开侧栏' : '收起侧栏'} onClick={onToggle}>
        <Icon name="nav" />
      </button>
      <div className="brand">
        <span className="mark" aria-hidden="true">律</span>
        <span className="brand-name">律金智库</span>
        <span className="space-switch ellipsis" title="当前私密空间（锁定后可切换）">{info.name}</span>
      </div>
      <button className="command-entry" onClick={onPalette} aria-label="打开命令面板（Ctrl+K）">
        <Icon name="search" />
        <span className="grow">搜索事务、页面，或直接提问</span>
        <span className="kbd">Ctrl K</span>
      </button>
      <div className="status-cluster" role="group" aria-label="隐私与连接">
        <button className="status-pill local" title="导入、识别、检索、重排、核查与计算都在本机完成" onClick={() => setPanel(panel === 'status' ? null : 'status')} aria-expanded={panel === 'status'}>
          <Icon name="shield" />
          本机处理
        </button>
        <button className={`status-pill${connection ? ' connected' : ''}`} title={connection ? `${connection.name} · ${connection.model_id}` : '未连接生成接口'} onClick={() => setPanel(panel === 'status' ? null : 'status')}>
          <span className="dot" />
          {connection ? connection.name : '生成：未连接'}
        </button>
        <button className="status-pill" title="联网检索与外部工具默认关闭" onClick={() => setPanel(panel === 'status' ? null : 'status')}>
          <Icon name="globe" />
          联网：关闭
        </button>
      </div>
      <button className="btn btn-subtle btn-icon task-button" aria-label={`任务中心：${running.length} 个进行中`} title="任务中心" onClick={() => setPanel(panel === 'tasks' ? null : 'tasks')} aria-expanded={panel === 'tasks'}>
        {running.length ? <span className="ring" style={{ ['--p' as string]: `${Math.max(progress, 8)}%` } as React.CSSProperties} /> : <Icon name="clock" />}
        {running.length > 0 && <span className="badge">{running.length}</span>}
      </button>
      <button className="btn btn-sm lock-button" title={lockHint ? `锁定（${lockHint}）` : '锁定'} onClick={lock}>
        <Icon name="lock" />
        锁定
      </button>
      {panel === 'status' && <StatusPanel onClose={() => setPanel(null)} />}
      {panel === 'tasks' && <TaskCenter onClose={() => setPanel(null)} />}
    </header>
  );
}

function StatusPanel({ onClose }: { onClose: () => void }) {
  const { connection, go } = useSession();
  const ref = useRef<HTMLDivElement>(null);
  useDismiss(ref, true, onClose, '.status-cluster');
  const jump = (view: View) => {
    onClose();
    go(view);
  };
  return (
    <div className="popover status-popover fade-in" ref={ref} role="dialog" aria-label="隐私与连接">
      <div className="status-line">
        <Icon name="shield" className="cap-supported" />
        <div>
          <div className="t-strong">本机处理</div>
          <div className="text-secondary t-caption">导入、识别、转写、检索、重排、逐句核查与计算都在这台电脑上完成；资料加密保存在本机。</div>
        </div>
      </div>
      <div className="status-line">
        <Icon name="robot" className={connection ? 'cap-supported' : 'cap-unknown'} />
        <div>
          <div className="t-strong">{connection ? `生成接口：${connection.name}` : '生成接口：未连接'}</div>
          <div className="text-secondary t-caption">
            {connection
              ? `${connection.host} · ${connection.model_id}。每次发送资料前都会先预览，再由系统对话框确认。`
              : '连接后可以生成带引用的回答；不连接也能检索原文、校对和计算。'}
          </div>
          <button className="btn btn-link t-caption" onClick={() => jump({ name: 'connections' })}>{connection ? '管理生成接口' : '连接生成接口'}</button>
        </div>
      </div>
      <div className="status-line">
        <Icon name="globe" className="cap-unknown" />
        <div>
          <div className="t-strong">联网：关闭</div>
          <div className="text-secondary t-caption">联网检索与外部工具尚未开放；开放后也默认关闭，每次使用前预览并授权。</div>
        </div>
      </div>
      <button className="btn btn-sm" onClick={() => jump({ name: 'settings', tab: 'security' })}>
        <Icon name="permissions" />
        查看授权记录
      </button>
    </div>
  );
}

function elapsed(iso: string): string {
  const seconds = Math.max(0, Math.round((Date.now() - new Date(iso).getTime()) / 1000));
  return seconds < 60 ? `${seconds} 秒` : `${Math.floor(seconds / 60)} 分 ${seconds % 60} 秒`;
}

function TaskCenter({ onClose }: { onClose: () => void }) {
  const { tasks } = useSession();
  const ref = useRef<HTMLDivElement>(null);
  const notice = useNotice();
  useDismiss(ref, true, onClose, '.task-button');
  const [, tick] = useState(0);
  useEffect(() => {
    const timer = window.setInterval(() => tick((t) => t + 1), 1000);
    return () => window.clearInterval(timer);
  }, []);
  const running = tasks.filter((t) => t.status === 'running');
  const recent = tasks.filter((t) => t.status !== 'running').sort((a, b) => b.started_at.localeCompare(a.started_at)).slice(0, 8);
  return (
    <div className="popover task-popover fade-in" ref={ref} role="dialog" aria-label="任务中心">
      <div className="t-strong" style={{ padding: '6px 8px' }}>进行中（{running.length}）</div>
      {running.length === 0 && <p className="text-tertiary t-caption" style={{ padding: '0 8px 8px' }}>没有进行中的任务。</p>}
      {running.map((t) => (
        <div key={t.task_id} className="task-row">
          <div style={{ minWidth: 0 }}>
            <div className="ellipsis">{skillName(t.skill)}</div>
            <div className="t-caption text-secondary">
              {t.progress ? `${stageName(t.progress.stage)}${t.progress.total > 0 ? ` · ${t.progress.done}/${t.progress.total}` : ''}` : '准备中'} · 已用 {elapsed(t.started_at)}
            </div>
          </div>
          <button className="btn btn-xs" onClick={() => api.cancelTask(t.task_id).then(() => notice('info', '已请求停止')).catch(() => {})}>
            <Icon name="stop" />
            停止
          </button>
          <div className="progress">
            <span style={{ width: `${t.progress && t.progress.total > 0 ? Math.round((t.progress.done / t.progress.total) * 100) : 6}%` }} />
          </div>
        </div>
      ))}
      {recent.length > 0 && <div className="t-strong" style={{ padding: '10px 8px 4px' }}>最近完成</div>}
      {recent.map((t) => (
        <div key={t.task_id} className="task-row">
          <div style={{ minWidth: 0 }}>
            <div className="ellipsis">{skillName(t.skill)}</div>
            <div className="t-caption text-tertiary">{relative(t.started_at)}{t.envelope?.warnings.length ? ` · ${t.envelope.warnings[0].message}` : ''}</div>
          </div>
          <div className="actions">
            <RetryButton task={t} />
            <span className={`chip ${t.status === 'completed' ? 'success' : t.status === 'partial' ? 'attention' : t.status === 'cancelled' ? '' : 'danger'}`.trim()}>{statusLabel(t.status)}</span>
          </div>
        </div>
      ))}
    </div>
  );
}

// ------------------------------------------------------------------ sidebar

function Sidebar({ collapsed }: { collapsed: boolean }) {
  const { view, go, matters } = useSession();
  const recent = [...matters].sort((a, b) => b.updated_at.localeCompare(a.updated_at)).slice(0, 5);
  const inRecent = view.name === 'matter' && recent.some((m) => m.id === view.id);
  const item = (id: View['name'], icon: IconName, label: string, current: boolean, later?: string) => (
    <button className="nav-item" aria-current={current ? 'page' : undefined} title={collapsed ? label : undefined} aria-label={collapsed ? label : undefined} onClick={() => go({ name: id } as View)}>
      <Icon name={icon} size="lg" />
      <span>{label}</span>
      {later && <span className="later">{later}</span>}
    </button>
  );
  return (
    <nav className="sidebar" aria-label="主导航">
      {item('home', 'home', '工作台', view.name === 'home')}
      {item('matters', 'folder', '我的事务', view.name === 'matters' || (view.name === 'matter' && (collapsed || !inRecent)))}
      {!collapsed && recent.length > 0 && (
        <div className="nav-sub">
          {recent.map((m) => (
            <button key={m.id} title={m.title} aria-current={view.name === 'matter' && view.id === m.id ? 'page' : undefined} onClick={() => go({ name: 'matter', id: m.id })}>
              {m.title}
            </button>
          ))}
        </div>
      )}
      {item('qa', 'chat', '问答与检索', view.name === 'qa')}
      {item('knowledge', 'library', '知识中心', view.name === 'knowledge', '后续')}
      <div className="nav-sep" />
      {item('connections', 'robot', '生成接口', view.name === 'connections')}
      {item('tools', 'connect', '工具与连接', view.name === 'tools', '后续')}
      <div className="sidebar-foot">{item('settings', 'settings', '设置', view.name === 'settings')}</div>
    </nav>
  );
}

// ------------------------------------------------------------ command palette

type Command = { group: string; icon: IconName; label: string; hint?: string; run: () => void };

function CommandPalette({ onClose }: { onClose: () => void }) {
  const { matters, go, view, newMatter, lock } = useSession();
  const [query, setQuery] = useState('');
  const [index, setIndex] = useState(0);
  const ref = useRef<HTMLDivElement>(null);
  useFocusTrap(ref, onClose);
  const q = query.trim();
  const needle = q.toLowerCase();
  const current = view.name === 'matter' ? matters.find((m) => m.id === view.id) : undefined;
  const commands: Command[] = [];
  if (q) {
    commands.push({ group: '提问', icon: 'chat', label: current ? `在“${current.title}”中提问：${q}` : `提问：${q}`, hint: 'Enter', run: () => go({ name: 'qa', matters: current ? [current.id] : undefined, question: q }) });
  }
  for (const m of matters.filter((m) => !needle || m.title.toLowerCase().includes(needle)).slice(0, 6)) {
    commands.push({ group: '事务', icon: 'folder', label: m.title, hint: m.candidate_facts ? `${m.candidate_facts} 条待校对` : undefined, run: () => go({ name: 'matter', id: m.id }) });
  }
  const pages: [View, IconName, string][] = [
    [{ name: 'home' }, 'home', '工作台'], [{ name: 'matters' }, 'folder', '我的事务'], [{ name: 'qa' }, 'chat', '问答与检索'], [{ name: 'connections' }, 'robot', '生成接口'],
    [{ name: 'settings', tab: 'security' }, 'settings', '设置：安全'], [{ name: 'settings', tab: 'appearance' }, 'settings', '设置：界面'], [{ name: 'settings', tab: 'backup' }, 'settings', '设置：备份与恢复'],
  ];
  for (const [target, icon, label] of pages.filter(([, , label]) => !needle || label.toLowerCase().includes(needle))) {
    commands.push({ group: '前往', icon, label, run: () => go(target) });
  }
  const actions: Command[] = [
    { group: '操作', icon: 'add', label: '新建事务', hint: 'Ctrl N', run: newMatter },
    { group: '操作', icon: 'lock', label: '锁定私密空间', hint: lockKeyLabel().replace(/\+/g, ' ') || undefined, run: lock },
  ];
  commands.push(...actions.filter((a) => !needle || a.label.includes(q)));
  const selected = Math.min(index, Math.max(commands.length - 1, 0));
  function run(command: Command | undefined) {
    if (!command) return;
    onClose();
    command.run();
  }
  function key(e: React.KeyboardEvent) {
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      setIndex((selected + 1) % Math.max(commands.length, 1));
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      setIndex((selected + commands.length - 1) % Math.max(commands.length, 1));
    } else if (e.key === 'Enter') {
      e.preventDefault();
      run(commands[selected]);
    }
  }
  let lastGroup = '';
  return (
    <div className="overlay" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className="palette fade-in" role="dialog" aria-label="命令面板" ref={ref}>
        <div className="palette-input">
          <Icon name="search" size="lg" />
          <input autoFocus value={query} onChange={(e) => { setQuery(e.target.value); setIndex(0); }} onKeyDown={key} placeholder="搜索事务、页面，或输入问题" aria-label="搜索或输入命令" aria-controls="palette-list" aria-activedescendant={commands.length ? `cmd-${selected}` : undefined} />
          <span className="kbd">Esc</span>
        </div>
        <div className="palette-list" role="listbox" id="palette-list">
          {commands.length === 0 && <div className="palette-empty">没有匹配的事务或操作。</div>}
          {commands.map((c, i) => {
            const head = c.group !== lastGroup ? <div className="palette-group">{c.group}</div> : null;
            lastGroup = c.group;
            return (
              <React.Fragment key={`${c.group}-${i}`}>
                {head}
                <div id={`cmd-${i}`} className="palette-item" role="option" aria-selected={i === selected} onMouseEnter={() => setIndex(i)} onMouseDown={(e) => e.preventDefault()} onClick={() => run(c)}>
                  <Icon name={c.icon} />
                  <span className="ellipsis">{c.label}</span>
                  {c.hint && <span className="hint">{c.hint}</span>}
                </div>
              </React.Fragment>
            );
          })}
        </div>
      </div>
    </div>
  );
}

// ---------------------------------------------------------- later (M2-B)

function Later({ page }: { page: 'knowledge' | 'tools' }) {
  const { go } = useSession();
  const knowledge = page === 'knowledge';
  return (
    <div className="page fade-in">
      <header className="page-head">
        <div className="titles">
          <h1 className="t-page">{knowledge ? '知识中心' : '工具与连接'}</h1>
          <p className="text-secondary" style={{ maxWidth: 'var(--reading-w)' }}>
            {knowledge ? '公共知识包（法规、术语与计算口径）的来源、版本与更新预览。' : '联网检索与外部工具（MCP）的服务、授权范围与撤销记录。'}
          </p>
        </div>
      </header>
      <Empty
        icon={knowledge ? 'library' : 'connect'}
        title="将在后续版本提供"
        action={<button className="btn" onClick={() => go({ name: 'qa' })}>去问答与检索</button>}
      >
        {knowledge ? '目前回答只依据你导入的资料；知识包需要联网更新，开放时会先预览差异并请你授权。' : '联网与外部工具目前全部关闭，本应用不会发起这类请求；开放后也默认关闭，每次使用前预览并授权。'}
      </Empty>
    </div>
  );
}

applyPreferences();

createRoot(document.getElementById('root')!).render(
  <Notices>
    <App />
  </Notices>,
);
