import { describe, expect, test } from "vitest";
import { createEmptySyncOutputFilter } from "./terminalFilter.js";

const encoder = new TextEncoder();
const decoder = new TextDecoder();

function encode(text) {
  return encoder.encode(text);
}

function decode(bytes) {
  return decoder.decode(bytes);
}

describe("createEmptySyncOutputFilter", () => {
  test("strips single and repeated empty ?2026h/?2026l pairs", () => {
    const f = createEmptySyncOutputFilter();
    expect(decode(f.filter(encode("\x1b[?2026h\x1b[?2026l")))).toBe("");
    expect(
      decode(
        f.filter(encode("before\x1b[?2026h\x1b[?2026l\x1b[?2026h\x1b[?2026lafter")),
      ),
    ).toBe("beforeafter");
  });

  test("strips empty pairs split arbitrarily across delta chunks", () => {
    const f = createEmptySyncOutputFilter();
    const input = encode("A\x1b[?2026h\x1b[?2026lB\x1b[?2026h\x1b[?2026lC");
    const chunks = [];
    for (let i = 0; i < input.length; i += 1) {
      const filtered = f.filter(input.subarray(i, i + 1));
      if (filtered.length > 0) chunks.push(decode(filtered));
    }
    expect(chunks.join("")).toBe("ABC");
  });

  test("preserves non-empty synchronized frames and partial mismatches", () => {
    const f = createEmptySyncOutputFilter();
    expect(
      decode(f.filter(encode("\x1b[?2026hhello world\x1b[?2026l"))),
    ).toBe("\x1b[?2026hhello world\x1b[?2026l");
    expect(
      decode(f.filter(encode("\x1b[?2025h中文输出\x1b[?2026h\x1b[?2026l!"))),
    ).toBe("\x1b[?2025h中文输出!");
  });

  test("preserves closing ?2026l when sync mode is already active", () => {
    const f = createEmptySyncOutputFilter();
    expect(
      decode(f.filter(encode("\x1b[?2026h\x1b[?2026h\x1b[?2026l"))),
    ).toBe("\x1b[?2026h\x1b[?2026l");
    expect(
      decode(f.filter(encode("\x1b[?2026htext\x1b[?2026h\x1b[?2026l"))),
    ).toBe("\x1b[?2026htext\x1b[?2026l");
    // Once closed back to idle, subsequent empty pulses are stripped again.
    expect(decode(f.filter(encode("\x1b[?2026h\x1b[?2026l")))).toBe("");
  });

  test("reset clears partial match buffer across terminal snapshots", () => {
    const f = createEmptySyncOutputFilter();
    expect(decode(f.filter(encode("\x1b[?2026")))).toBe("");
    f.reset();
    expect(decode(f.filter(encode("h\x1b[?2026l")))).toBe("h\x1b[?2026l");
  });
});
