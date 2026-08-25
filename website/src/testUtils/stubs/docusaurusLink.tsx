import type { AnchorHTMLAttributes, ReactNode } from 'react';

// Test-only stand-in for @docusaurus/Link, which Docusaurus's own webpack
// resolver provides at build time and isn't a real npm package vitest/Vite
// can resolve. Aliased in vitest.config.ts.
export default function Link({
  children,
  to,
  ...rest
}: AnchorHTMLAttributes<HTMLAnchorElement> & { children: ReactNode; to: string }): ReactNode {
  return (
    <a href={to} {...rest}>
      {children}
    </a>
  );
}
