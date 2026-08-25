import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';

const STORAGE_KEY = 'llmenv-docs-midi-enabled';

// vi.mock factories below are hoisted above regular declarations, so the
// values they close over must come from vi.hoisted (which hoists with them)
// rather than a plain top-level const.
const { FakePlayer, instrumentMock } = vi.hoisted(() => {
  class FakePlayer {
    isPlaying = vi.fn(() => false);
    on = vi.fn();
    loadArrayBuffer = vi.fn();
    play = vi.fn();
    stop = vi.fn();
  }
  const instrumentMock = vi.fn(async () => ({
    play: vi.fn(() => ({ stop: vi.fn() })),
  }));
  return { FakePlayer, instrumentMock };
});

vi.mock('midi-player-js', () => ({
  default: { Player: FakePlayer },
}));

vi.mock('soundfont-player', () => ({
  default: { instrument: instrumentMock },
}));

import RetroMidiPlayer from '../../src/theme/RetroMidiPlayer';

describe('RetroMidiPlayer', () => {
  beforeEach(() => {
    localStorage.clear();
    instrumentMock.mockClear();
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => ({
        ok: true,
        arrayBuffer: async () => new ArrayBuffer(8),
      })),
    );
    class FakeAudioContext {
      resume = vi.fn(async () => undefined);
      close = vi.fn(async () => undefined);
    }
    vi.stubGlobal('AudioContext', FakeAudioContext);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it('starts enabled when nothing is stored', async () => {
    render(<RetroMidiPlayer />);

    await waitFor(() =>
      expect(screen.getByRole('button', { name: 'Mute background music' })).toBeInTheDocument(),
    );
  });

  it('starts muted when localStorage previously recorded "off"', async () => {
    localStorage.setItem(STORAGE_KEY, 'off');

    render(<RetroMidiPlayer />);

    await waitFor(() =>
      expect(screen.getByRole('button', { name: 'Play background music' })).toBeInTheDocument(),
    );
  });

  it('toggling mutes, updates the label, and persists the choice', async () => {
    render(<RetroMidiPlayer />);
    const button = await screen.findByRole('button', { name: 'Mute background music' });

    fireEvent.click(button);

    expect(await screen.findByRole('button', { name: 'Play background music' })).toBeInTheDocument();
    expect(localStorage.getItem(STORAGE_KEY)).toBe('off');
  });

  it('toggling twice re-enables and persists that choice', async () => {
    render(<RetroMidiPlayer />);
    const button = await screen.findByRole('button', { name: 'Mute background music' });

    fireEvent.click(button);
    await screen.findByRole('button', { name: 'Play background music' });
    fireEvent.click(screen.getByRole('button', { name: 'Play background music' }));

    expect(await screen.findByRole('button', { name: 'Mute background music' })).toBeInTheDocument();
    expect(localStorage.getItem(STORAGE_KEY)).toBe('on');
  });

  it('loads both the melodic and percussion instruments when enabled', async () => {
    render(<RetroMidiPlayer />);

    await waitFor(() => expect(instrumentMock).toHaveBeenCalledTimes(2));
    expect(instrumentMock).toHaveBeenCalledWith(expect.anything(), 'acoustic_grand_piano');
    expect(instrumentMock).toHaveBeenCalledWith(expect.anything(), 'synth_drum');
  });

  it('still briefly requests instruments once when starting muted, per the documented SSR-parity tradeoff', async () => {
    // `enabled` starts `true` to match server-rendered output and is only
    // corrected from localStorage after mount (see the "ponytail" comment in
    // RetroMidiPlayer.tsx), so a returning muted visitor can still trigger one
    // instrument load before the correction lands. Accepted, not a bug.
    localStorage.setItem(STORAGE_KEY, 'off');

    render(<RetroMidiPlayer />);

    await screen.findByRole('button', { name: 'Play background music' });
    await waitFor(() => expect(instrumentMock).toHaveBeenCalledTimes(2));
  });

  it('logs and does not crash when the audio setup chain fails', async () => {
    const consoleErrorSpy = vi.spyOn(console, 'error').mockImplementation(() => undefined);
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => ({
        ok: false,
        status: 404,
        arrayBuffer: async () => new ArrayBuffer(0),
      })),
    );

    render(<RetroMidiPlayer />);

    await waitFor(() =>
      expect(consoleErrorSpy).toHaveBeenCalledWith(
        'llmenv-docs: MIDI theme failed to start',
        expect.any(Error),
      ),
    );
    // The button stays interactive -- a failed load doesn't crash the component.
    expect(screen.getByRole('button', { name: 'Mute background music' })).toBeInTheDocument();

    consoleErrorSpy.mockRestore();
  });

  it('falls back to enabled when localStorage.getItem throws', async () => {
    const getItemSpy = vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => {
      throw new Error('storage unavailable');
    });

    render(<RetroMidiPlayer />);

    await waitFor(() =>
      expect(screen.getByRole('button', { name: 'Mute background music' })).toBeInTheDocument(),
    );

    getItemSpy.mockRestore();
  });

  it('still flips the displayed state when localStorage.setItem throws', async () => {
    const setItemSpy = vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
      throw new Error('storage unavailable');
    });

    render(<RetroMidiPlayer />);
    const button = await screen.findByRole('button', { name: 'Mute background music' });

    fireEvent.click(button);

    // The preference just won't persist across visits -- the toggle itself
    // still works even though the write failed.
    expect(
      await screen.findByRole('button', { name: 'Play background music' }),
    ).toBeInTheDocument();

    setItemSpy.mockRestore();
  });
});
