declare module 'soundfont-player' {
  export interface InstrumentPlayOptions {
    duration?: number;
    gain?: number;
  }

  export interface InstrumentNode {
    stop(when?: number): void;
  }

  export interface Instrument {
    play(note: string, when?: number, options?: InstrumentPlayOptions): InstrumentNode;
  }

  interface Soundfont {
    instrument(audioContext: AudioContext, name: string): Promise<Instrument>;
  }

  const soundfont: Soundfont;
  export default soundfont;
}
