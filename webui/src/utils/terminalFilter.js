const SYNC_START = new Uint8Array([0x1b, 0x5b, 0x3f, 0x32, 0x30, 0x32, 0x36, 0x68]); // \x1b[?2026h
const SYNC_END = new Uint8Array([0x1b, 0x5b, 0x3f, 0x32, 0x30, 0x32, 0x36, 0x6c]); // \x1b[?2026l
const EMPTY_PAIR = new Uint8Array(SYNC_START.length + SYNC_END.length);
EMPTY_PAIR.set(SYNC_START, 0);
EMPTY_PAIR.set(SYNC_END, SYNC_START.length);

function emptyBytes() {
  return new Uint8Array(0);
}

/**
 * Stateful byte-stream filter that removes empty DEC 2026 synchronized output
 * pairs (`\x1b[?2026h\x1b[?2026l`) — even when split across consecutive delta
 * chunks — so xterm DomRenderer does not rebuild every row on 30Hz no-op
 * sync pulses. Non-empty `\x1b[?2026h...\x1b[?2026l` frames pass through.
 */
export function createEmptySyncOutputFilter() {
  /** Number of leading bytes of `EMPTY_PAIR` currently matched cleanly. */
  let matched = 0;

  return {
    filter(chunk) {
      if (!ArrayBuffer.isView(chunk) || chunk.byteLength === 0) {
        return emptyBytes();
      }
      const bytes =
        chunk instanceof Uint8Array
          ? chunk
          : new Uint8Array(chunk.buffer, chunk.byteOffset, chunk.byteLength);

      const out = [];

      const consumeByte = (byte) => {
        while (true) {
          if (byte === EMPTY_PAIR[matched]) {
            matched += 1;
            if (matched === EMPTY_PAIR.length) {
              // Drop the complete empty `\x1b[?2026h\x1b[?2026l` pair.
              matched = 0;
            }
            return;
          }

          if (matched === 0) {
            out.push(byte);
            return;
          }

          if (matched < SYNC_START.length) {
            // Partial `\x1b[?2026h` prefix failed: flush buffered prefix and
            // restart cleanly (checking if this byte itself begins ESC).
            for (let i = 0; i < matched; i += 1) {
              out.push(SYNC_START[i]);
            }
            matched = 0;
            continue;
          }

          // Matched full SYNC_START plus `suffixMatched` bytes of SYNC_END.
          const suffixMatched = matched - SYNC_START.length;

          // Flush the buffered `SYNC_START`, then re-feed `SYNC_END[0..suffixMatched]`
          // + `byte` so back-to-back `\x1b[?2026h\x1b[?2026h\x1b[?2026l` or any
          // nested prefix still catches the trailing empty pair.
          for (let i = 0; i < SYNC_START.length; i += 1) {
            out.push(SYNC_START[i]);
          }
          matched = 0;
          for (let i = 0; i < suffixMatched; i += 1) {
            consumeByte(SYNC_END[i]);
          }
        }
      };

      for (let i = 0; i < bytes.length; i += 1) {
        consumeByte(bytes[i]);
      }

      return Uint8Array.from(out);
    },

    reset() {
      matched = 0;
    },
  };
}
