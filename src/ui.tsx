// Small shared building blocks: notices, modal, tabs and async helpers.
import React, { createContext, useCallback, useContext, useEffect, useRef, useState } from 'react';

type Notice = { id: number; tone: 'ok' | 'error' | 'info'; text: string };
const NoticeContext = createContext<(tone: Notice['tone'], text: string) => void>(() => {});

export function Notices({ children }: { children: React.ReactNode }) {
  const [items, setItems] = useState<Notice[]>([]);
  const counter = useRef(0);
  const push = useCallback((tone: Notice['tone'], text: string) => {
    const id = ++counter.current;
    setItems((list) => [...list.slice(-3), { id, tone, text }]);
    window.setTimeout(() => setItems((list) => list.filter((n) => n.id !== id)), tone === 'error' ? 9000 : 4500);
  }, []);
  return (
    <NoticeContext.Provider value={push}>
      {children}
      <div className="notices" role="status" aria-live="polite">
        {items.map((n) => (
          <div key={n.id} className={`notice notice-${n.tone}`}>
            <span className="notice-mark" aria-hidden>{n.tone === 'ok' ? '✓' : n.tone === 'error' ? '!' : 'i'}</span>
            <span>{n.text}</span>
          </div>
        ))}
      </div>
    </NoticeContext.Provider>
  );
}

export const useNotice = () => useContext(NoticeContext);

/** Run an async action with a busy flag and an error notice. */
export function useAction() {
  const notice = useNotice();
  const [busy, setBusy] = useState(false);
  const run = useCallback(
    async <T,>(work: () => Promise<T>, success?: string): Promise<T | undefined> => {
      setBusy(true);
      try {
        const value = await work();
        if (success) notice('ok', success);
        return value;
      } catch (error) {
        notice('error', error instanceof Error ? error.message : String(error));
        return undefined;
      } finally {
        setBusy(false);
      }
    },
    [notice],
  );
  return { busy, run };
}

export function useLoad<T>(load: () => Promise<T>, deps: unknown[]): { data: T | undefined; error: string; reload: () => void; loading: boolean } {
  const [data, setData] = useState<T>();
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(true);
  const [tick, setTick] = useState(0);
  useEffect(() => {
    let alive = true;
    setLoading(true);
    load()
      .then((value) => alive && (setData(value), setError('')))
      .catch((e: unknown) => alive && setError(e instanceof Error ? e.message : String(e)))
      .finally(() => alive && setLoading(false));
    return () => {
      alive = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [...deps, tick]);
  return { data, error, loading, reload: () => setTick((t) => t + 1) };
}

export function Modal({ title, onClose, children, wide }: { title: string; onClose: () => void; children: React.ReactNode; wide?: boolean }) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const key = (e: KeyboardEvent) => e.key === 'Escape' && onClose();
    window.addEventListener('keydown', key);
    ref.current?.querySelector<HTMLElement>('input,select,textarea,button')?.focus();
    return () => window.removeEventListener('keydown', key);
  }, [onClose]);
  return (
    <div className="overlay" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className={`modal${wide ? ' modal-wide' : ''}`} role="dialog" aria-modal="true" aria-label={title} ref={ref}>
        <header className="modal-head">
          <h2>{title}</h2>
          <button className="icon" onClick={onClose} aria-label="关闭">×</button>
        </header>
        <div className="modal-body">{children}</div>
      </div>
    </div>
  );
}

export function Tabs<T extends string>({ value, onChange, items }: { value: T; onChange: (value: T) => void; items: [T, string, number?][] }) {
  return (
    <div className="tabs" role="tablist">
      {items.map(([id, label, count]) => (
        <button key={id} role="tab" aria-selected={value === id} className={value === id ? 'tab active' : 'tab'} onClick={() => onChange(id)}>
          {label}
          {count !== undefined && <span className="count">{count}</span>}
        </button>
      ))}
    </div>
  );
}

export function Field({ label, children, hint }: { label: string; children: React.ReactNode; hint?: string }) {
  return (
    <label className="field">
      <span className="field-label">{label}</span>
      {children}
      {hint && <span className="field-hint">{hint}</span>}
    </label>
  );
}

export function Empty({ title, children }: { title: string; children?: React.ReactNode }) {
  return (
    <div className="empty">
      <strong>{title}</strong>
      {children && <p>{children}</p>}
    </div>
  );
}

export function Chip({ tone = 'neutral', children, title }: { tone?: 'neutral' | 'ok' | 'warn' | 'danger' | 'info' | 'gold'; children: React.ReactNode; title?: string }) {
  return (
    <span className={`chip chip-${tone}`} title={title}>
      {children}
    </span>
  );
}

export function Amount({ value, direction, currency }: { value: string; direction?: 'in' | 'out'; currency?: string }) {
  const sign = direction === 'out' ? '−' : direction === 'in' ? '+' : '';
  return (
    <span className={`amount${direction ? ` amount-${direction}` : ''}`}>
      {sign}
      {value}
      {currency && currency !== 'CNY' ? ` ${currency}` : ''}
    </span>
  );
}

export function Busy({ label = '处理中…' }: { label?: string }) {
  return (
    <div className="busy" role="status">
      <span className="spinner" aria-hidden />
      {label}
    </div>
  );
}

export function ErrorLine({ text }: { text: string }) {
  return text ? (
    <p className="error-line" role="alert">
      {text}
    </p>
  ) : null;
}
