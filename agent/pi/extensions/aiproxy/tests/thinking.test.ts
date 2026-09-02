import { describe, it, expect, vi } from "vitest";
import { ThinkScanner } from "../thinking.ts";
import type { AssistantMessageEvent, AssistantMessageEventStream } from "@earendil-works/pi-ai";
import { cleanStream } from "../thinking.ts";

// Mock createAssistantMessageEventStream to return a simple push-based stream
vi.mock("@earendil-works/pi-ai", () => ({
  createAssistantMessageEventStream() {
    const events: AssistantMessageEvent[] = [];
    let resolve: (() => void) | null = null;
    let streamDone = false;
    const stream: AssistantMessageEventStream = {
      push(ev: AssistantMessageEvent) {
        events.push(ev);
        if (ev.type === "done" || ev.type === "error") streamDone = true;
        resolve?.();
        resolve = null;
      },
      [Symbol.asyncIterator]() {
        let i = 0;
        return {
          async next() {
            while (i >= events.length && !streamDone) {
              await new Promise<void>((r) => (resolve = r));
            }
            if (i >= events.length) return { value: undefined, done: true };
            return { value: events[i++], done: false };
          },
        };
      },
    };
    return stream;
  },
}));
vi.mock("@earendil-works/pi-ai/compat", () => ({
  getApiProvider() {
    return undefined;
  },
}));

function ev(type: string, fields: Record<string, unknown> = {}) {
  return {
    type,
    partial: {
      role: "assistant" as const,
      content: [],
      api: "openai-completions",
      provider: "test",
      model: "test",
      usage: {},
      stopReason: null,
      timestamp: 0,
    },
    ...fields,
  } as unknown as AssistantMessageEvent;
}

async function collect(stream: AssistantMessageEventStream): Promise<AssistantMessageEvent[]> {
  const events: AssistantMessageEvent[] = [];
  for await (const e of stream) events.push(e);
  return events;
}

/** Create a mock base stream from an array of events. */
function baseStream(events: AssistantMessageEvent[]): AssistantMessageEventStream {
  let i = 0;
  return {
    push() {},
    [Symbol.asyncIterator]() {
      return {
        async next() {
          if (i >= events.length) return { value: undefined, done: true };
          return { value: events[i++], done: false };
        },
      };
    },
  } as unknown as AssistantMessageEventStream;
}

describe("cleanStream", () => {
  it("passes through start and done events", async () => {
    const base = baseStream([
      ev("start"),
      ev("text_start"),
      ev("text_delta", { delta: "hello" }),
      ev("text_end", { content: "hello" }),
      ev("done", {
        reason: "stop",
        message: {
          role: "assistant",
          content: [{ type: "text", text: "hello" }],
          api: "openai-completions",
          provider: "test",
          model: "test",
          usage: {},
          stopReason: "stop",
          timestamp: 0,
        },
      }),
    ]);
    const out = cleanStream(base);
    const events = await collect(out);
    const types = events.map((e) => e.type);
    expect(types).toContain("start");
    expect(types).toContain("done");
  });

  it("merges consecutive thinking blocks into one", async () => {
    const base = baseStream([
      ev("start"),
      ev("thinking_start"),
      ev("thinking_delta", { delta: "part1" }),
      ev("thinking_end", {}),
      ev("thinking_start"),
      ev("thinking_delta", { delta: "part2" }),
      ev("thinking_end", {}),
      ev("text_start"),
      ev("text_delta", { delta: "answer" }),
      ev("text_end", { content: "answer" }),
      ev("done", {
        reason: "stop",
        message: {
          role: "assistant",
          content: [],
          api: "openai-completions",
          provider: "test",
          model: "test",
          usage: {},
          stopReason: "stop",
          timestamp: 0,
        },
      }),
    ]);
    const out = cleanStream(base);
    const events = await collect(out);
    const thinkingDeltas = events.filter((e) => e.type === "thinking_delta");
    // Should have exactly one thinking_start/end pair, not two
    const thinkingStarts = events.filter((e) => e.type === "thinking_start");
    const thinkingEnds = events.filter((e) => e.type === "thinking_end");
    expect(thinkingStarts.length).toBe(1);
    expect(thinkingEnds.length).toBe(1);
  });

  it("strips <think> tags from text deltas", async () => {
    const base = baseStream([
      ev("start"),
      ev("text_start"),
      ev("text_delta", { delta: "<think>think</think>answer" }),
      ev("text_end", { content: "<think>think</think>answer" }),
      ev("done", {
        reason: "stop",
        message: {
          role: "assistant",
          content: [],
          api: "openai-completions",
          provider: "test",
          model: "test",
          usage: {},
          stopReason: "stop",
          timestamp: 0,
        },
      }),
    ]);
    const out = cleanStream(base);
    const events = await collect(out);
    const textDeltas = events.filter((e) => e.type === "text_delta");
    const allText = textDeltas.map((e: any) => e.delta).join("");
    expect(allText).not.toContain("<think>");
    expect(allText).not.toContain("awesome");
    expect(allText).toContain("answer");
  });

  it("suppresses prefix re-streams when reasoning fields alternate", async () => {
    const base = baseStream([
      ev("start"),
      ev("thinking_start"),
      ev("thinking_delta", { delta: "shared prefix" }),
      ev("thinking_end", {}),
      ev("thinking_start"),
      ev("thinking_delta", { delta: "shared prefix extended" }),
      ev("thinking_end", {}),
      ev("text_start"),
      ev("text_delta", { delta: "done" }),
      ev("text_end", { content: "done" }),
      ev("done", {
        reason: "stop",
        message: {
          role: "assistant",
          content: [],
          api: "openai-completions",
          provider: "test",
          model: "test",
          usage: {},
          stopReason: "stop",
          timestamp: 0,
        },
      }),
    ]);
    const out = cleanStream(base);
    const events = await collect(out);
    const thinkingDeltas = events.filter((e) => e.type === "thinking_delta");
    const allThinking = thinkingDeltas.map((e: any) => e.delta).join("");
    // The prefix "shared prefix" should appear only once
    const first = allThinking.indexOf("shared prefix");
    const second = allThinking.indexOf("shared prefix", first + 1);
    // If second exists, it should be "shared prefix" followed by different content
    // Either way, no exact duplicate of the full string
    expect(allThinking.split("shared prefix").length).toBeLessThanOrEqual(2);
  });
});

describe("ThinkScanner", () => {
  it("splits text before think tag", () => {
    const s = new ThinkScanner();
    const r = s.feed("hello <think>think");
    expect(r.text).toBe("hello ");
    expect(r.think).toBe("think");
  });

  it("splits text after think tag", () => {
    const s = new ThinkScanner();
    const r1 = s.feed("<think>think");
    const r2 = s.feed("</think>answer");
    expect(r1.think + r2.think).toBe("think");
    expect(r2.text).toBe("answer");
  });

  it("handles tag split across two feeds", () => {
    const s = new ThinkScanner();
    const r1 = s.feed("<think>thi");
    const r2 = s.feed("nking</think>ok");
    expect(r1.think + r2.think).toBe("thinking");
    expect(r2.text).toBe("ok");
  });

  it("handles multiple think blocks", () => {
    const s = new ThinkScanner();
    s.feed("<think>first</think>");
    const r2 = s.feed("<think>second");
    const r3 = s.feed("</think>final");
    expect(r2.think + r3.think).toBe("second");
    expect(r3.text).toBe("final");
  });

  it("flush returns remaining think content when unterminated", () => {
    const s = new ThinkScanner();
    const r1 = s.feed("<think>partial");
    const r2 = s.flush();
    expect(r1.think + r2.think).toBe("partial");
  });

  it("flush returns remaining text content when no open tag", () => {
    const s = new ThinkScanner();
    const r1 = s.feed("hello");
    const r2 = s.flush();
    expect(r1.text + r2.text).toBe("hello");
  });

  it("empty input produces empty output", () => {
    const s = new ThinkScanner();
    const r = s.feed("");
    expect(r.text).toBe("");
    expect(r.think).toBe("");
  });

  it("tag at very start of stream", () => {
    const s = new ThinkScanner();
    const r = s.feed("<think>thinking</think>");
    expect(r.think).toBe("thinking");
    expect(r.text).toBe("");
  });

  it("tag at very end of stream", () => {
    const s = new ThinkScanner();
    const r = s.feed("<think>tagged</think>");
    expect(r.text).toBe("");
    expect(r.think).toBe("tagged");
  });
});
