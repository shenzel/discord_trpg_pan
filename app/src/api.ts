import type { Api } from "./types";
import { createMockApi } from "./mock";

async function createTauriApi(): Promise<Api> {
  const { invoke } = await import("@tauri-apps/api/core");
  const { listen } = await import("@tauri-apps/api/event");
  return {
    getState: () => invoke("get_state"),
    onState: (cb) => void listen("state", (e) => cb(e.payload as never)),
    setPhase: (phase) => void invoke("set_phase", { phase }),
    setRole: (userId, role) => void invoke("set_role", { userId, role }),
    reorder: (userId, index) => void invoke("reorder", { userId, index }),
    resetScenario: () => void invoke("reset_scenario"),
    setGmMode: (on) => void invoke("set_gm_mode", { on }),
    relogin: (side) => void invoke("relogin", { side: side ?? "main" }),
    setGap: (gap) => void invoke("set_gap", { gap }),
    setSpan: (lo, hi) => void invoke("set_span", { lo, hi }),
    setVolume: (userId, volume) => void invoke("set_volume", { userId, volume }),
    addSubAccount: () => void invoke("add_sub_account"),
    removeSubAccount: () => void invoke("remove_sub_account"),
    quit: () => void invoke("quit_app"),
    setSelfMute: (on) => void invoke("set_self_mute", { on }),
    requestMute: (userId, on) => void invoke("request_mute", { userId, on }),
    setInputDevice: (id) => void invoke("set_input_device", { id }),
    setOutputDevice: (id) => void invoke("set_output_device", { id }),
    refreshDevices: () => void invoke("refresh_devices"),
    openLogDir: () => void invoke("open_log_dir"),
  };
}

/** Tauriの中ならバックエンドへ、ブラウザ（npm run dev）ならモックへつなぐ。 */
export function createApi(): Promise<Api> {
  return "__TAURI_INTERNALS__" in window ? createTauriApi() : Promise.resolve(createMockApi());
}
