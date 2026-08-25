import path from 'node:path';
import { defineConfig } from 'vitest/config';
import react from '@vitejs/plugin-react';

export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: {
      '@site': import.meta.dirname,
      // Docusaurus's own webpack resolver provides these at build time; they
      // aren't real npm packages, so Vite can't resolve them under test.
      '@docusaurus/Link': path.resolve(import.meta.dirname, 'test/stubs/docusaurusLink.tsx'),
      '@docusaurus/useDocusaurusContext': path.resolve(
        import.meta.dirname,
        'test/stubs/useDocusaurusContext.ts',
      ),
      '@docusaurus/useBaseUrl': path.resolve(import.meta.dirname, 'test/stubs/useBaseUrl.ts'),
      '@theme/Layout': path.resolve(import.meta.dirname, 'test/stubs/themeLayout.tsx'),
    },
  },
  test: {
    environment: 'jsdom',
    globals: false,
    setupFiles: ['./test/setup.ts'],
  },
});
