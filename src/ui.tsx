// Shared building blocks (design §8): toasts, dialogs, tabs, fields, chips,
// callouts, amounts, empty states, skeletons and async helpers.
import React, { createContext, useCallback, useContext, useEffect, useId, useRef, useState } from 'react';
import { money } from './api';
import { Icon, type IconName } from './icons';

type Toast = { id: number; tone: 'ok' | 'error' | 'info'; text: string };
const ToastContext = createContext<(tone: Toast['tone'], text: string) => void>(() => {});

/** Bottom-centre toasts, at most two at a time (design 8.9). */
export function Notices({ children }: { children: React.ReactNode }) {
  const [items, setItems] = useState<Toast[]>([]);
  const counter = useRef(0);
  const dismiss = useCallback((id: number) => setItems((list) => list.filter((n) => n.id !== id)), []);
  const push = useCallback(
    (tone: Toast['tone'], text: string) => {
      const id = ++counter.current;
      setItems((list) => [...list, { id, tone, text }].slice(-2));
      window.setTimeout(() => dismiss(id), tone === 'error' ? 12000 : 4000);
    },
    [dismiss],
  );
  return (
    <ToastContext.Provider value={push}>
      {children}
      <div className="toasts" role="status" aria-live="polite">
        {items.map((n) => (
          <div key={n.id} className={`toast fade-in ${n.tone}`}>
            <Icon name={n.tone === 'ok' ? 'check' : n.tone === 'error' ? 'error' : 'info'} />
            <span>{n.text}</span>
            <button className="toast-close" aria-label="关闭通知" onClick={() => dismiss(n.id)}>
              <Icon name="close" />
            </button>
          </div>
        ))}
      </div>
    </ToastContext.Provider>
  );
}

export const useNotice = () => useContext(ToastContext);

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
  const reload = useCallback(() => setTick((t) => t + 1), []);
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
  return { data, error, loading, reload };
}

/** True once `ms` have passed; skeletons appear only for slow loads (design 8.20). */
export function useDelayed(ms = 300): boolean {
  const [late, setLate] = useState(false);
  useEffect(() => {
    const timer = window.setTimeout(() => setLate(true), ms);
    return () => window.clearTimeout(timer);
  }, [ms]);
  return late;
}

const FOCUSABLE = 'button:not(:disabled),input:not(:disabled),select:not(:disabled),textarea:not(:disabled),[href],[tabindex]:not([tabindex="-1"])';

/** Keep Tab inside `root`; Escape calls `onEscape`. Focus returns on unmount. */
export function useFocusTrap(root: React.RefObject<HTMLElement | null>, onEscape: () => void) {
  const escape = useRef(onEscape);
  escape.current = onEscape;
  useEffect(() => {
    const previous = document.activeElement as HTMLElement | null;
    const key = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.stopPropagation();
        e.preventDefault();
        escape.current();
      } else if (e.key === 'Tab' && root.current) {
        const items = [...root.current.querySelectorAll<HTMLElement>(FOCUSABLE)];
        if (!items.length) return;
        const first = items[0];
        const last = items[items.length - 1];
        if (e.shiftKey && document.activeElement === first) {
          e.preventDefault();
          last.focus();
        } else if (!e.shiftKey && document.activeElement === last) {
          e.preventDefault();
          first.focus();
        }
      }
    };
    // Capture phase: an open dialog handles Escape before any page shortcut.
    window.addEventListener('keydown', key, true);
    return () => {
      window.removeEventListener('keydown', key, true);
      previous?.focus?.();
    };
  }, [root]);
}

export function Modal({ title, onClose, children, wide, icon, footer }: { title: string; onClose: () => void; children: React.ReactNode; wide?: boolean; icon?: IconName; footer?: React.ReactNode }) {
  const ref = useRef<HTMLDivElement>(null);
  const id = useId();
  useFocusTrap(ref, onClose);
  useEffect(() => {
    const target = ref.current?.querySelector<HTMLElement>('.dialog-body input:not([type="checkbox"]),.dialog-body select,.dialog-body textarea') ?? ref.current?.querySelector<HTMLElement>('.dialog-body button,.dialog-foot button');
    target?.focus();
  }, []);
  return (
    <div className="overlay center" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className={`dialog fade-in${wide ? ' wide' : ''}`} role="dialog" aria-modal="true" aria-labelledby={id} ref={ref}>
        <div className="dialog-head">
          {icon && <Icon name={icon} size="lg" />}
          <h2 className="t-title" id={id}>{title}</h2>
          <button className="btn btn-subtle btn-icon dialog-close" onClick={onClose} aria-label="关闭">
            <Icon name="close" />
          </button>
        </div>
        <div className="dialog-body">{children}</div>
        {footer && <div className="dialog-foot">{footer}</div>}
      </div>
    </div>
  );
}

/** Underline tabs; counts appear only where something needs attention (design 8.4). */
export function Tabs<T extends string>({ value, onChange, items, label }: { value: T; onChange: (value: T) => void; items: [T, string, number?, ('neutral' | 'attention')?][]; label?: string }) {
  const ref = useRef<HTMLDivElement>(null);
  function move(e: React.KeyboardEvent) {
    if (e.key !== 'ArrowRight' && e.key !== 'ArrowLeft') return;
    e.preventDefault();
    const index = items.findIndex(([id]) => id === value);
    const next = items[(index + (e.key === 'ArrowRight' ? 1 : items.length - 1)) % items.length][0];
    onChange(next);
    window.requestAnimationFrame(() => ref.current?.querySelector<HTMLElement>('[aria-selected="true"]')?.focus());
  }
  return (
    <div className="tabs" role="tablist" aria-label={label} ref={ref} onKeyDown={move}>
      {items.map(([id, text, count, tone]) => (
        <button key={id} role="tab" aria-selected={value === id} tabIndex={value === id ? 0 : -1} className="tab" onClick={() => onChange(id)}>
          {text}
          {count ? <span className={`count${tone === 'neutral' ? ' neutral' : ''}`}>{count}</span> : null}
        </button>
      ))}
    </div>
  );
}

export function Segmented<T extends string>({ value, onChange, items, label }: { value: T; onChange: (value: T) => void; items: [T, string, boolean?][]; label: string }) {
  return (
    <div className="segmented" role="group" aria-label={label}>
      {items.map(([id, text, disabled]) => (
        <button key={id} type="button" aria-pressed={value === id} disabled={disabled} onClick={() => onChange(id)}>
          {text}
        </button>
      ))}
    </div>
  );
}

export function Field({ label, children, hint, error, className }: { label: string; children: React.ReactNode; hint?: React.ReactNode; error?: string; className?: string }) {
  return (
    <label className={`field${className ? ` ${className}` : ''}`}>
      <span className="field-label">{label}</span>
      {children}
      {error ? (
        <span className="field-error">
          <Icon name="error" />
          {error}
        </span>
      ) : (
        hint && <span className="field-hint">{hint}</span>
      )}
    </label>
  );
}

/** Like Field for controls that are not a single input (e.g. segmented
 *  buttons), so clicking the caption does not press the first button. */
export function FieldGroup({ label, children, hint }: { label: string; children: React.ReactNode; hint?: React.ReactNode }) {
  return (
    <div className="field" role="group" aria-label={label}>
      <span className="field-label">{label}</span>
      {children}
      {hint && <span className="field-hint">{hint}</span>}
    </div>
  );
}

/** Icon, one-line title, one-line explanation and at most one action (design 8.19). */
export function Empty({ title, children, icon = 'info', action }: { title: string; children?: React.ReactNode; icon?: IconName; action?: React.ReactNode }) {
  return (
    <div className="empty">
      <Icon name={icon} size="xl" />
      <strong>{title}</strong>
      {children && <p>{children}</p>}
      {action}
    </div>
  );
}

export type Tone = 'neutral' | 'ok' | 'warn' | 'danger' | 'info' | 'gold' | 'accent' | 'warning' | 'outline';
const CHIP: Record<Tone, string> = { neutral: '', ok: 'success', warn: 'attention', gold: 'attention', danger: 'danger', info: 'info', accent: 'accent', warning: 'warning', outline: 'outline' };

export function Chip({ tone = 'neutral', icon, children, title }: { tone?: Tone; icon?: IconName; children: React.ReactNode; title?: string }) {
  return (
    <span className={`chip ${CHIP[tone]}`.trim()} title={title}>
      {icon && <Icon name={icon} />}
      {children}
    </span>
  );
}

const CALLOUT_ICON: Record<string, IconName> = { info: 'info', attention: 'highlight', warning: 'warning', danger: 'error', success: 'check' };

export function Callout({ tone = 'info', icon, title, children, action, className }: { tone?: 'info' | 'attention' | 'warning' | 'danger' | 'success'; icon?: IconName; title?: React.ReactNode; children?: React.ReactNode; action?: React.ReactNode; className?: string }) {
  return (
    <div className={`callout${tone === 'info' ? '' : ` ${tone}`}${className ? ` ${className}` : ''}`}>
      <Icon name={icon ?? CALLOUT_ICON[tone]} />
      <div className="body">
        {title && <strong>{title}</strong>}
        {children && <span className="text-secondary">{children}</span>}
      </div>
      {action}
    </div>
  );
}

/** Amounts (design 8.16): income "+" in teal, expense "−" in body colour, transfers grey. */
export function Amount({ value, direction, currency, kind }: { value: string; direction?: 'in' | 'out'; currency?: string; kind?: string }) {
  const transfer = kind === 'transfer';
  const sign = direction === 'out' ? '−' : direction === 'in' ? '+' : '';
  return (
    <span className={`amount${direction === 'in' && !transfer ? ' in' : ''}${transfer ? ' transfer' : ''}`}>
      {sign}
      {money(value)}
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

/** Placeholder rows shown only after 300 ms (design 8.20). */
export function Skeleton({ rows = 3, label = '正在加载' }: { rows?: number; label?: string }) {
  const late = useDelayed(300);
  if (!late) return null;
  return (
    <div className="skeleton-block" role="status" aria-label={label}>
      {Array.from({ length: rows }, (_, i) => (
        <div key={i} className="skeleton" style={{ height: 14, width: `${92 - i * 9}%` }} />
      ))}
    </div>
  );
}

export function ErrorLine({ text }: { text: string }) {
  return text ? (
    <p className="error-line" role="alert">
      <Icon name="error" />
      <span>{text}</span>
    </p>
  ) : null;
}

/** Close a popover on outside click or Escape; clicks inside `ignore`
 *  (the popover's own toggle buttons) are left to those buttons. */
export function useDismiss(ref: React.RefObject<HTMLElement | null>, open: boolean, onClose: () => void, ignore?: string) {
  const close = useRef(onClose);
  close.current = onClose;
  useEffect(() => {
    if (!open) return;
    const down = (e: MouseEvent) => {
      const target = e.target as Element;
      if (ignore && target.closest?.(ignore)) return;
      if (ref.current && !ref.current.contains(target)) close.current();
    };
    const key = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.stopPropagation();
        close.current();
      }
    };
    window.addEventListener('mousedown', down);
    window.addEventListener('keydown', key, true);
    return () => {
      window.removeEventListener('mousedown', down);
      window.removeEventListener('keydown', key, true);
    };
  }, [ref, open, ignore]);
}
