import { describe, expect, it } from 'vitest';
import { render, screen } from '@testing-library/react';

import Home from '../index';

describe('Home', () => {
  it('renders the site title and tagline from Docusaurus context', () => {
    render(<Home />);

    expect(screen.getByRole('heading', { level: 1, name: 'llmenv' })).toBeInTheDocument();
    expect(
      screen.getByText('direnv for Claude Code and other AI tools.'),
    ).toBeInTheDocument();
  });

  it('renders both hero call-to-action links to the right pages', () => {
    render(<Home />);

    expect(screen.getByRole('link', { name: 'Get Started' })).toHaveAttribute(
      'href',
      '/docs/getting-started',
    );
    expect(screen.getByRole('link', { name: 'Why llmenv?' })).toHaveAttribute(
      'href',
      '/docs/philosophy',
    );
  });

  it('renders every feature title', () => {
    render(<Home />);

    for (const title of [
      'Per-environment config',
      'One config, every tool',
      'Integrated memory',
      'Context management',
      'First-class MCP & LSP',
      'Plugins & skills, one bundle',
    ]) {
      expect(screen.getByRole('heading', { level: 3, name: title })).toBeInTheDocument();
    }
  });
});
