import type { ReactNode } from 'react';

export default function Root({ children }: { children: ReactNode }): ReactNode {
  return (
    <>
      <div className="retro-marquee-track" aria-hidden="true">
        <span className="retro-marquee-text">
          ★☆★ WELCOME TO THE LLMENV DOCS ★☆★ BEST VIEWED IN NETSCAPE NAVIGATOR 4.0 AT 800x600 ★☆★
          SIGN THE GUESTBOOK ★☆★
        </span>
      </div>
      <div className="retro-divider" role="presentation" />
      {children}
    </>
  );
}
