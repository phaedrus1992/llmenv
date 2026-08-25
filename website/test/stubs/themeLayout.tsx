import type { ReactNode } from 'react';

// Test-only stand-in for @theme/Layout (see docusaurusLink.tsx for why this
// needs an alias instead of a real import).
export default function Layout({ children }: { children: ReactNode }): ReactNode {
  return <>{children}</>;
}
