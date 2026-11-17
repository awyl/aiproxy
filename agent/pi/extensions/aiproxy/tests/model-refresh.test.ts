import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { parseProxyModels, registerModelRefresh } from '../model-refresh.ts';
import type { ProxyModel } from '../provider.ts';

const first: ProxyModel[] = [{id:'default/fast',target:'openai/old',surface:'chat'}];
const next: ProxyModel[] = [{id:'default/fast',target:'openai-codex/new',surface:'responses'}];

function harness(fetchModels = vi.fn().mockResolvedValue(next)) {
  const handlers = new Map<string, Function>();
  const pi = {on:vi.fn((name:string,fn:Function)=>handlers.set(name,fn))};
  let idle = true;
  const ctx = {isIdle:()=>idle,model:{provider:'aiproxy',id:'default/fast'},hasUI:true,ui:{notify:vi.fn()}};
  const apply = vi.fn();
  registerModelRefresh(pi as never, first, fetchModels, apply);
  const emit = async (name:string) => { await handlers.get(name)?.({},ctx); };
  return {apply,ctx,emit,fetchModels,busy:()=>{idle=false;},idle:()=>{idle=true;}};
}

beforeEach(()=>vi.useFakeTimers());
afterEach(()=>{vi.clearAllTimers();vi.useRealTimers();});

describe('model refresh lifecycle',()=>{
  it('polls at 60 seconds, not earlier, and does not reapply unchanged data',async()=>{
    const h=harness(); await h.emit('session_start');
    await vi.advanceTimersByTimeAsync(59_999); expect(h.fetchModels).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(1); expect(h.apply).toHaveBeenCalledWith(next);
    await vi.advanceTimersByTimeAsync(60_000); expect(h.apply).toHaveBeenCalledTimes(1);
  });
  it('treats reordered responses as unchanged',async()=>{
    const h=harness(vi.fn().mockResolvedValue([{id:'other/m',surface:'chat'},...first]));
    await h.emit('session_start'); await vi.advanceTimersByTimeAsync(60_000);
    h.fetchModels.mockResolvedValue([...first,{id:'other/m',surface:'chat'}]);
    await vi.advanceTimersByTimeAsync(60_000); expect(h.apply).toHaveBeenCalledTimes(1);
  });
  it('defers a busy update until agent_settled without another network request',async()=>{
    const h=harness(); await h.emit('session_start'); h.busy();
    await vi.advanceTimersByTimeAsync(60_000); expect(h.apply).not.toHaveBeenCalled();
    h.idle(); await h.emit('agent_settled'); expect(h.apply).toHaveBeenCalledWith(next);
    expect(h.fetchModels).toHaveBeenCalledTimes(1);
  });
  it('keeps working models on failures and warns once per outage',async()=>{
    const h=harness(vi.fn().mockRejectedValue(new Error('offline')));
    await h.emit('session_start'); await vi.advanceTimersByTimeAsync(120_000);
    expect(h.apply).not.toHaveBeenCalled(); expect(h.ctx.ui.notify).toHaveBeenCalledTimes(1);
    h.fetchModels.mockResolvedValue(next); await vi.advanceTimersByTimeAsync(60_000);
    expect(h.apply).toHaveBeenCalledWith(next);
    h.fetchModels.mockRejectedValue(new Error('offline')); await vi.advanceTimersByTimeAsync(60_000);
    expect(h.ctx.ui.notify).toHaveBeenCalledTimes(2);
  });
  it('applies removals and warns when the selected model disappears',async()=>{
    const h=harness(vi.fn().mockResolvedValue([])); await h.emit('session_start');
    await vi.advanceTimersByTimeAsync(60_000); expect(h.apply).toHaveBeenCalledWith([]);
    expect(h.ctx.ui.notify).toHaveBeenCalledWith(expect.stringContaining('default/fast'), 'warning');
  });
});

describe('catalog validation',()=>{
  it('preserves valid discovery data',()=>{
    expect(parseProxyModels({data:first})).toEqual(first);
  });
  it.each([{}, {data:null}, {data:{}}, {data:[{}]}, {data:[{id:''}]},
    {data:[{id:'a/m',surface:'invalid'}]}, {data:[{id:'a/m',target:42}]},
    {data:[{id:'a/m'},{id:'a/m'}]}])('rejects malformed catalog %j',body=>{
    expect(()=>parseProxyModels(body)).toThrow();
  });
});

describe('refresh cancellation',()=>{
  it('does not overlap requests and aborts at ten seconds',async()=>{
    let signal: AbortSignal | undefined;
    const h=harness(vi.fn().mockImplementation((s:AbortSignal)=>{signal=s;return new Promise(()=>{});}));
    await h.emit('session_start'); await vi.advanceTimersByTimeAsync(60_000);
    expect(signal?.aborted).toBe(false);
    await vi.advanceTimersByTimeAsync(9_999); expect(signal?.aborted).toBe(false);
    await vi.advanceTimersByTimeAsync(1); expect(signal?.aborted).toBe(true);
    await vi.advanceTimersByTimeAsync(60_000); expect(h.fetchModels).toHaveBeenCalledTimes(1);
  });
  it('shutdown aborts a request and ignores a late response',async()=>{
    let finish: (value:ProxyModel[])=>void = ()=>{};
    let signal: AbortSignal | undefined;
    const h=harness(vi.fn().mockImplementation((s:AbortSignal)=>{signal=s;return new Promise(r=>{finish=r;});}));
    await h.emit('session_start'); await vi.advanceTimersByTimeAsync(60_000);
    await h.emit('session_shutdown'); expect(signal?.aborted).toBe(true);
    finish(next); await vi.advanceTimersByTimeAsync(120_000);
    expect(h.apply).not.toHaveBeenCalled(); expect(h.fetchModels).toHaveBeenCalledTimes(1);
  });
  it('repeated session_start replaces its timer',async()=>{
    const h=harness(); await h.emit('session_start'); await h.emit('session_start');
    await vi.advanceTimersByTimeAsync(60_000); expect(h.fetchModels).toHaveBeenCalledTimes(1);
    await h.emit('session_shutdown'); await vi.advanceTimersByTimeAsync(120_000);
    expect(h.fetchModels).toHaveBeenCalledTimes(1);
  });
  it('ignores a pending update if the latest catalog returns to the current state',async()=>{
    const h=harness(); await h.emit('session_start'); h.busy();
    await vi.advanceTimersByTimeAsync(60_000);
    h.fetchModels.mockResolvedValue(first); await vi.advanceTimersByTimeAsync(60_000);
    h.idle(); await h.emit('agent_settled'); expect(h.apply).not.toHaveBeenCalled();
  });
});

describe("timeout recovery",()=>{
  it("retries normally after an aborted request and publishes a later success",async()=>{
    const fetchModels=vi.fn()
      .mockImplementationOnce((signal:AbortSignal)=>new Promise((_,reject)=>{
        signal.addEventListener("abort",()=>reject(new Error("request timed out")),{once:true});
      }))
      .mockResolvedValue(next);
    const h=harness(fetchModels);
    await h.emit("session_start"); await vi.advanceTimersByTimeAsync(70_000);
    expect(h.ctx.ui.notify).toHaveBeenCalledTimes(1);
    expect(h.apply).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(50_000);
    expect(h.fetchModels).toHaveBeenCalledTimes(2);
    expect(h.apply).toHaveBeenCalledWith(next);
  });
  it("cannot publish a late response from an earlier session",async()=>{
    let finish: (models:ProxyModel[])=>void=()=>{};
    const fetchModels=vi.fn()
      .mockImplementationOnce(()=>new Promise(resolve=>{finish=resolve;}))
      .mockResolvedValue(next);
    const h=harness(fetchModels);
    await h.emit("session_start"); await vi.advanceTimersByTimeAsync(60_000);
    await h.emit("session_start"); await vi.advanceTimersByTimeAsync(60_000);
    expect(h.apply).toHaveBeenCalledWith(next);
    finish([{id:"obsolete/model",surface:"chat"}]);
    await vi.advanceTimersByTimeAsync(0);
    expect(h.apply).toHaveBeenCalledTimes(1);
  });
});
