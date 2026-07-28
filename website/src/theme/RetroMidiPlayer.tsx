import { useCallback, useEffect, useState, type ReactNode } from 'react';
import MidiPlayer from 'midi-player-js';
import Soundfont, { type Instrument } from 'soundfont-player';
import useBaseUrl from '@docusaurus/useBaseUrl';

const MELODIC_INSTRUMENT = 'acoustic_grand_piano';
// This soundfont pack (gleitz/midi-js-soundfonts) only ships the 128 standard
// GM melodic instruments (see MusyngKite/names.json) — there's no dedicated
// drum-kit instrument, so 'percussion' 404s. synth_drum (GM program 119) is
// the closest available substitute for channel 10 in this simplified 2-voice
// mapping; it plays one timbre per note rather than a real multi-drum kit.
const PERCUSSION_INSTRUMENT = 'synth_drum';
// General MIDI reserves channel 10 for drums/percussion (1-indexed, as midi-player-js reports it).
const PERCUSSION_CHANNEL = 10;
const ENABLED_STORAGE_KEY = 'llmenv-docs-midi-enabled';

function readStoredEnabled(): boolean {
  try {
    return window.localStorage.getItem(ENABLED_STORAGE_KEY) !== 'off';
  } catch {
    return true;
  }
}

// ponytail: one voice for melody/harmony + one for drums, not full General MIDI
// per-channel instrument mapping (Program Change tracking, 128-name lookup).
// Good enough for a background Easter egg; upgrade if the arrangement needs
// its real per-track instrumentation.
export default function RetroMidiPlayer(): ReactNode {
  const midiUrl = useBaseUrl('/audio/theme.mid');
  // Starts true to match server-rendered output; corrected from localStorage right
  // after mount (see visitor-counter hook in NavbarItem/ComponentTypes.tsx for the
  // same SSR-safe pattern). ponytail: a returning muted visitor may briefly start
  // one fetch/instrument-load before this correction lands — not worth an
  // AbortController for a decorative feature.
  const [enabled, setEnabled] = useState(true);

  useEffect(() => {
    setEnabled(readStoredEnabled());
  }, []);

  const toggle = useCallback(() => {
    setEnabled((previous) => {
      const next = !previous;
      try {
        window.localStorage.setItem(ENABLED_STORAGE_KEY, next ? 'on' : 'off');
      } catch {
        // localStorage unavailable — preference just won't persist across visits.
      }
      return next;
    });
  }, []);

  useEffect(() => {
    if (!enabled) return undefined;

    let cancelled = false;
    let player: MidiPlayer.Player | undefined;
    let audioContext: AudioContext | undefined;
    const activeNotes = new Map<string, ReturnType<Instrument['play']>>();
    let removeInteractionListeners: (() => void) | undefined;

    async function setup(): Promise<void> {
      audioContext = new AudioContext();
      const [melodic, percussion] = await Promise.all([
        Soundfont.instrument(audioContext, MELODIC_INSTRUMENT),
        Soundfont.instrument(audioContext, PERCUSSION_INSTRUMENT),
      ]);
      if (cancelled) return;

      const response = await fetch(midiUrl);
      if (!response.ok) {
        throw new Error(`Failed to fetch MIDI theme: ${response.status} ${midiUrl}`);
      }
      const arrayBuffer = await response.arrayBuffer();
      if (cancelled) return;

      player = new MidiPlayer.Player();
      player.on('midiEvent', (event: MidiPlayer.Event) => {
        if (cancelled || !event.noteName) return;
        const instrument = event.channel === PERCUSSION_CHANNEL ? percussion : melodic;
        const key = `${event.channel}:${event.noteNumber}`;

        if (event.name === 'Note on' && (event.velocity ?? 0) > 0) {
          activeNotes.set(key, instrument.play(event.noteName));
        } else if (event.name === 'Note off' || event.name === 'Note on') {
          activeNotes.get(key)?.stop();
          activeNotes.delete(key);
        }
      });
      player.loadArrayBuffer(arrayBuffer);

      // Browsers block audio autoplay until the user has interacted with the
      // page — try immediately, and fall back to starting on first click/key.
      const tryPlay = () => {
        void audioContext?.resume().then(() => {
          if (!cancelled && player && !player.isPlaying()) player.play();
        });
      };
      tryPlay();
      document.addEventListener('pointerdown', tryPlay, { once: true });
      document.addEventListener('keydown', tryPlay, { once: true });
      removeInteractionListeners = () => {
        document.removeEventListener('pointerdown', tryPlay);
        document.removeEventListener('keydown', tryPlay);
      };
    }

    setup().catch((err: unknown) => {
      console.error('llmenv-docs: MIDI theme failed to start', err);
    });

    return () => {
      cancelled = true;
      removeInteractionListeners?.();
      player?.stop();
      void audioContext?.close();
    };
  }, [enabled, midiUrl]);

  return (
    <button
      type="button"
      className="retro-midi-toggle"
      onClick={toggle}
      aria-label={enabled ? 'Mute background music' : 'Play background music'}
      title={enabled ? 'Mute background music' : 'Play background music'}
    >
      {enabled ? '🔊' : '🔇'}
    </button>
  );
}
