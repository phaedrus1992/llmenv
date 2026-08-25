import { describe, expect, it, vi } from 'vitest';
import { render, screen } from '@testing-library/react';

vi.mock('../../src/theme/RetroMidiPlayer', () => ({
  default: () => <button type="button">mock-midi-player</button>,
}));

import Root from '../../src/theme/Root';

describe('Root', () => {
  it('renders children alongside the retro chrome', () => {
    render(
      <Root>
        <p>page content</p>
      </Root>,
    );

    expect(screen.getByText('page content')).toBeInTheDocument();
  });

  it('renders the retro marquee text', () => {
    render(<Root>{null}</Root>);

    expect(screen.getByText(/WELCOME TO THE LLMENV DOCS/)).toBeInTheDocument();
  });

  it('renders the MIDI player', () => {
    render(<Root>{null}</Root>);

    expect(screen.getByText('mock-midi-player')).toBeInTheDocument();
  });
});
