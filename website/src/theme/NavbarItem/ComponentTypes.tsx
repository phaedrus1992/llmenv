import { useEffect, useState, type ReactNode } from 'react';
import ComponentTypes from '@theme-original/NavbarItem/ComponentTypes';

const VISITOR_COUNTER_KEY = 'llmenv-docs-visitor-count';
const VISITOR_COUNTER_START = 3;

// ponytail: fake counter, but real per-browser — starts at 3 and climbs by one
// each full page load, stored in this browser's localStorage. GitHub Pages has
// no backend, so this is the only "increments for real" option that adds no
// third-party tracker. Returns null until the effect resolves so the UI shows
// nothing rather than flashing the starting count before correcting itself.
function useVisitorCount(): number | null {
  const [count, setCount] = useState<number | null>(null);

  useEffect(() => {
    try {
      const raw = window.localStorage.getItem(VISITOR_COUNTER_KEY);
      const previous = raw === null ? VISITOR_COUNTER_START - 1 : Number(raw);
      const next = Number.isFinite(previous) ? previous + 1 : VISITOR_COUNTER_START;
      window.localStorage.setItem(VISITOR_COUNTER_KEY, String(next));
      setCount(next);
    } catch (err: unknown) {
      // localStorage unavailable (private browsing, disabled storage) — show the starting count.
      console.debug('llmenv-docs: visitor counter storage unavailable', err);
      setCount(VISITOR_COUNTER_START);
    }
  }, []);

  return count;
}

function RetroStatusNavbarItem(): ReactNode {
  const visitorCount = useVisitorCount();

  return (
    <div className="navbar__item retro-navbar-status">
      <span className="retro-construction">
        <span className="retro-blink">🚧</span> UNDER CONSTRUCTION{' '}
        <span className="retro-blink">🚧</span>
      </span>
      {visitorCount !== null && (
        <span className="retro-hit-counter">
          YOU ARE VISITOR # {String(visitorCount).padStart(6, '0')}
        </span>
      )}
    </div>
  );
}

export default {
  ...ComponentTypes,
  'custom-retroStatus': RetroStatusNavbarItem,
};
