import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it, vi } from "vitest";
import { createAgentSession, DefaultResourceLoader, ModelRuntime, SessionManager, SettingsManager } from "@earendil-works/pi-coding-agent";
import { registerProxyProvider } from "../provider.ts";

describe("real pi selected-model refresh", () => {
  it("keeps the selected alias while replacing its wire driver and capabilities", async () => {
    const dir = mkdtempSync(join(tmpdir(), "aiproxy-refresh-sdk-"));
    const fetchImpl = vi.fn()
      .mockResolvedValueOnce({ok:true,json:async()=>({data:[{id:"default/planner",target:"opencode-go/old",surface:"chat"}]})})
      .mockResolvedValue({ok:true,json:async()=>({data:[{id:"default/planner",target:"openai-codex/new",surface:"responses"}]})});
    const settings = SettingsManager.inMemory();
    const runtime = await ModelRuntime.create({
      authPath:join(dir,"auth.json"),modelsPath:null,modelsStorePath:join(dir,"models-store.json"),
      allowModelNetwork:false,refreshOnCreate:false,
    });
    const loader = new DefaultResourceLoader({
      cwd:dir,agentDir:dir,settingsManager:settings,noExtensions:true,noSkills:true,
      noPromptTemplates:true,noThemes:true,noContextFiles:true,
      extensionFactories:[async pi => {
        await registerProxyProvider(pi,{base:"http://mock/v1",apiKey:"test-key"},{
          fetchImpl:fetchImpl as never,
          catalog:new Map([["openai-codex/new",{reasoning:true,contextWindow:272_000,maxTokens:32_000}]]),
        });
      }],
    });
    let session: Awaited<ReturnType<typeof createAgentSession>>["session"] | undefined;
    try {
      await loader.reload();
      expect(loader.getExtensions().errors).toEqual([]);
      ({session} = await createAgentSession({
        cwd:dir,agentDir:dir,modelRuntime:runtime,settingsManager:settings,
        resourceLoader:loader,sessionManager:SessionManager.inMemory(dir),noTools:"all",
      }));
      await session.bindExtensions({});
      const original = runtime.getModel("aiproxy","default/planner")!;
      expect(original).toBeDefined();
      await session.setModel(original);
      expect(session.model?.id).toBe("default/planner");
      expect(session.model?.reasoning).toBe(false);
      vi.useFakeTimers();
      await session.extensionRunner.emit({type:"session_start"});
      await vi.advanceTimersByTimeAsync(60_000);
      expect(session.model).not.toBe(original);
      expect(session.model).toEqual(runtime.getModel("aiproxy","default/planner"));
      expect(session.model).toMatchObject({
        id:"default/planner",provider:"aiproxy",api:"openai-responses",
        reasoning:true,contextWindow:272_000,maxTokens:32_000,
      });
      expect(runtime.getAvailableSnapshot().find(m=>m.provider==="aiproxy" && m.id==="default/planner"))
        .toMatchObject({api:"openai-responses",contextWindow:272_000});
      console.log("real pi selection after refresh:", session.model?.id, session.model?.api, session.model?.contextWindow);
      const refreshed = session.model;
      fetchImpl.mockResolvedValue({ok:true,json:async()=>({data:[]})});
      const warning = vi.spyOn(console,"warn").mockImplementation(()=>{});
      try {
        await vi.advanceTimersByTimeAsync(60_000);
        expect(runtime.getModel("aiproxy","default/planner")).toBeUndefined();
        expect(session.model).toBe(refreshed);
        expect(warning).toHaveBeenCalledWith(expect.stringContaining("No fallback was selected"));
      } finally { warning.mockRestore(); }
    } finally {
      await session?.extensionRunner.emit({type:"session_shutdown"});
      session?.dispose();
      vi.clearAllTimers();vi.useRealTimers();
      rmSync(dir,{recursive:true,force:true});
    }
  }, 15_000);
});
