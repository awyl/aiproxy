import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import type { ProxyModel } from "./provider.ts";

export function parseProxyModels(value: unknown): ProxyModel[] {
  const data = (value as { data?: unknown } | null)?.data;
  if (!Array.isArray(data)) throw new Error("invalid aiproxy model catalog");
  const seen = new Set<string>();
  return data.map((entry: unknown) => {
    if (!entry || typeof entry !== "object") throw new Error("invalid aiproxy model entry");
    const m = entry as Record<string, unknown>;
    if (typeof m.id !== "string" || !m.id || seen.has(m.id) ||
        (m.target !== undefined && (typeof m.target !== "string" || !m.target)) ||
        (m.display_name != null && typeof m.display_name !== "string") ||
        (m.surface !== undefined && !["chat","messages","responses","embedding","unknown"].includes(m.surface as string))) {
      throw new Error("invalid or duplicate aiproxy model entry");
    }
    seen.add(m.id);
    return {
      id: m.id,
      target: m.target as string | undefined,
      surface: m.surface as string | undefined,
      display_name: (m.display_name ?? undefined) as string | undefined,
    };
  });
}

function fingerprint(models: ProxyModel[]): string {
  return JSON.stringify(models.map(m => [m.id, m.target, m.surface, m.display_name])
    .sort((a,b) => a[0]!.localeCompare(b[0]!)));
}

export function registerModelRefresh(
  pi: ExtensionAPI,
  initial: ProxyModel[],
  fetchModels: (signal: AbortSignal) => Promise<ProxyModel[]>,
  apply: (models: ProxyModel[]) => void,
): void {
  let current = fingerprint(initial);
  let pending: ProxyModel[] | undefined;
  let timer: ReturnType<typeof setInterval> | undefined;
  let request: AbortController | undefined;
  let generation = 0;
  let active = false;
  let failed = false;
  let missingSelection: string | undefined;

  function warn(ctx: ExtensionContext, message: string): void {
    if (ctx.hasUI) ctx.ui.notify(message, "warning");
    else console.warn(message);
  }

  function reportFailure(ctx: ExtensionContext, error: unknown): void {
    if (failed) return;
    failed = true;
    warn(ctx, "[aiproxy] model refresh failed; keeping the last catalog: " +
      (error instanceof Error ? error.message : String(error)));
  }

  function publish(ctx: ExtensionContext): void {
    if (!active || !pending || !ctx.isIdle()) return;
    const models = pending;
    const next = fingerprint(models);
    if (next !== current) {
      apply(models);
      current = next;
      const selected = ctx.model;
      const missing = selected?.provider === "aiproxy" && !models.some(m => m.id === selected.id)
        ? selected.id : undefined;
      if (missing && missing !== missingSelection) {
        warn(ctx, "[aiproxy] selected model " + missing + " is unavailable; choose another model. No fallback was selected.");
      }
      missingSelection = missing;
    }
    pending = undefined;
  }

  async function poll(ctx: ExtensionContext): Promise<void> {
    if (!active || request) return;
    const epoch = generation;
    const controller = new AbortController();
    request = controller;
    const deadline = setTimeout(() => controller.abort(), 10_000);
    try {
      const models = await fetchModels(controller.signal);
      if (!active || generation !== epoch || controller.signal.aborted) return;
      pending = models;
      publish(ctx);
      failed = false;
    } catch (error) {
      if (active && generation === epoch) reportFailure(ctx, error);
    } finally {
      clearTimeout(deadline);
      if (request === controller) request = undefined;
    }
  }

  function stop(): void {
    active = false;
    generation++;
    if (timer) clearInterval(timer);
    timer = undefined;
    request?.abort();
    request = undefined;
    pending = undefined;
  }

  pi.on("session_start", (_event, ctx) => {
    stop();
    active = true;
    timer = setInterval(() => { void poll(ctx); }, 60_000);
    timer.unref?.();
  });
  pi.on("agent_settled", (_event, ctx) => {
    try { publish(ctx); } catch (error) { reportFailure(ctx, error); }
  });
  pi.on("session_shutdown", stop);
}
