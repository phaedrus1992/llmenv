import { useEffect, useState, type ReactNode } from 'react';

const VISITOR_COUNTER_KEY = 'llmenv-docs-visitor-count';
const VISITOR_COUNTER_START = 3;

// ponytail: fake counter, but real per-browser — starts at 3 and climbs by one
// each full page load, stored in this browser's localStorage. GitHub Pages has
// no backend, so this is the only "increments for real" option that adds no
// third-party tracker.
function useVisitorCount(): number {
  const [count, setCount] = useState(VISITOR_COUNTER_START);

  useEffect(() => {
    try {
      const raw = window.localStorage.getItem(VISITOR_COUNTER_KEY);
      const previous = raw === null ? VISITOR_COUNTER_START - 1 : Number(raw);
      const next = Number.isFinite(previous) ? previous + 1 : VISITOR_COUNTER_START;
      window.localStorage.setItem(VISITOR_COUNTER_KEY, String(next));
      setCount(next);
    } catch {
      // localStorage unavailable (private browsing, disabled storage) — keep the starting count.
    }
  }, []);

  return count;
}

export default function Root({ children }: { children: ReactNode }): ReactNode {
  const visitorCount = useVisitorCount();

  return (
    <>
      <div className="retro-marquee-track" aria-hidden="true">
        <span className="retro-marquee-text">
          ★☆★ WELCOME TO THE LLMENV DOCS ★☆★ BEST VIEWED IN NETSCAPE NAVIGATOR 4.0 AT 800x600 ★☆★
          SIGN THE GUESTBOOK ★☆★
        </span>
      </div>
      <div className="retro-status-bar">
        <span className="retro-construction">
          <span className="retro-blink">🚧</span> UNDER CONSTRUCTION{' '}
          <span className="retro-blink">🚧</span>
        </span>
        <span className="retro-hit-counter">
          YOU ARE VISITOR # {String(visitorCount).padStart(6, '0')}
        </span>
      </div>
      <div className="retro-divider" role="presentation" />
      {children}
    </>
  );
}
