import type { ReactNode } from 'react';

// ponytail: static gag number, not a real visitor tracker.
const VISITOR_COUNT = '004217';

export default function Root({ children }: { children: ReactNode }): ReactNode {
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
        <span className="retro-hit-counter">YOU ARE VISITOR # {VISITOR_COUNT}</span>
      </div>
      <div className="retro-divider" role="presentation" />
      {children}
    </>
  );
}
