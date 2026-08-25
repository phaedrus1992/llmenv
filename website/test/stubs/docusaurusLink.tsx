import type { AnchorHTMLAttributes, ReactNode } from 'react';
import useBaseUrl from './useBaseUrl';

// Test-only stand-in for @docusaurus/Link, which Docusaurus's own webpack
// resolver provides at build time and isn't a real npm package vitest/Vite
// can resolve. Aliased in vitest.config.mts. Real Link base-url-prefixes any
// local (root-relative) `to`, so this does too -- otherwise a baseUrl
// regression wouldn't show up in an asserted href.
export default function Link({
  children,
  to,
  ...rest
}: AnchorHTMLAttributes<HTMLAnchorElement> & { children: ReactNode; to: string }): ReactNode {
  const href = to.startsWith('/') && !to.startsWith('//') ? useBaseUrl(to) : to;
  return (
    <a href={href} {...rest}>
      {children}
    </a>
  );
}
