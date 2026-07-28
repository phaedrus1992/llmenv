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

  export interface InstrumentOptions {
    format?: 'mp3' | 'ogg';
    soundfont?: 'MusyngKite' | 'FluidR3_GM';
    nameToUrl?(name: string, soundfont?: string, format?: string): string;
    destination?: AudioNode;
    gain?: number;
  }

  interface Soundfont {
    instrument(
      audioContext: AudioContext,
      name: string,
      options?: InstrumentOptions,
    ): Promise<Instrument>;
  }

  const soundfont: Soundfont;
  export default soundfont;
}
