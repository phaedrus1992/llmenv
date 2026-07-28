import { useEffect, type ReactNode } from 'react';
import MidiPlayer from 'midi-player-js';
import Soundfont, { type Instrument } from 'soundfont-player';
import useBaseUrl from '@docusaurus/useBaseUrl';

const MELODIC_INSTRUMENT = 'acoustic_grand_piano';
const PERCUSSION_INSTRUMENT = 'percussion';
// General MIDI reserves channel 10 for drums/percussion (1-indexed, as midi-player-js reports it).
const PERCUSSION_CHANNEL = 10;

// ponytail: one voice for melody/harmony + one for drums, not full General MIDI
// per-channel instrument mapping (Program Change tracking, 128-name lookup).
// Good enough for a background Easter egg; upgrade if the arrangement needs
// its real per-track instrumentation.
export default function RetroMidiPlayer(): ReactNode {
  const midiUrl = useBaseUrl('/audio/theme.mid');

  useEffect(() => {
    let cancelled = false;
    const audioContext = new AudioContext();
    const activeNotes = new Map<string, ReturnType<Instrument['play']>>();
    let removeInteractionListeners: (() => void) | undefined;

    async function setup(): Promise<void> {
      const [melodic, percussion] = await Promise.all([
        Soundfont.instrument(audioContext, MELODIC_INSTRUMENT),
        Soundfont.instrument(audioContext, PERCUSSION_INSTRUMENT),
      ]);
      if (cancelled) return;

      const response = await fetch(midiUrl);
      const arrayBuffer = await response.arrayBuffer();
      if (cancelled) return;

      const player = new MidiPlayer.Player();
      player.on('midiEvent', (event: MidiPlayer.Event) => {
        if (!event.noteName) return;
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
        void audioContext.resume().then(() => {
          if (!cancelled && !player.isPlaying()) player.play();
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

    void setup();

    return () => {
      cancelled = true;
      removeInteractionListeners?.();
      void audioContext.close();
    };
  }, [midiUrl]);

  return null;
}
