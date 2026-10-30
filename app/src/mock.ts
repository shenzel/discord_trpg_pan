// ブラウザで画面だけ確認するためのモック。配置ロジックは簡略版（本物は Rust の session.rs）。
import type { Api, MemberView, Phase, Role, Snapshot } from "./types";

export function createMockApi(): Api {
  let phase = (new URLSearchParams(location.search).get("phase") ?? "before") as Phase;
  const people: { id: string; name: string; role: Role; presence: MemberView["presence"]; grace: number | null }[] = [
    { id: "1", name: "たなか", role: "pl", presence: "present", grace: null },
    { id: "2", name: "Suzuki_the_Brave", role: "pl", presence: "present", grace: null },
    { id: "3", name: "さとうさん（GM）", role: "gm", presence: "present", grace: null },
    { id: "4", name: "やまもとやまもと", role: "pl", presence: "grace", grace: 42 },
    { id: "5", name: "見学のひと", role: "spectator", presence: "present", grace: null },
  ];
  let order = people.map((p) => p.id);
  let gmMode = false;
  // ?span=-0.2,0.2 で L 端・R 端を指定して開く
  let span = (new URLSearchParams(location.search).get("span")?.split(",").map(Number) ?? [-1, 1]) as [number, number];
  // −1〜+1 の位置を L 端〜R 端の中へ
  const fit = (p: number) => span[0] + ((p + 1) / 2) * (span[1] - span[0]);
  const volumes = new Map<string, number>();
  let dual = new URLSearchParams(location.search).has("dual");
  const readOnly = new URLSearchParams(location.search).has("pl");
  let listener: (s: Snapshot) => void = () => {};

  const seated = (r: Role) => phase === "after" || (phase === "during" && r === "pl");
  const snapshot = (): Snapshot => {
    const seats = order.filter((id) => seated(people.find((p) => p.id === id)!.role));
    const n = seats.length;
    return {
      status: { kind: "connected" },
      voice: { kind: "in_call" },
      gm_mode: gmMode,
      remote_gm: readOnly ? { id: "3", name: "さとうさん（GM）" } : null,
      read_only: readOnly,
      warnings: [],
      side: "main",
      self_muted: false,
      gap: 0.2,
      span,
      sub: null,
      audio: {
        inputs: [
          { id: "i1", name: "default (マイク (Virtual Desktop Audio))", default: true },
          { id: "i2", name: "INPUT (2- Volt 1)", default: false },
        ],
        outputs: [{ id: "o1", name: "default (ヘッドホン (Volt 1))", default: true }],
        current_input: "i2",
        current_output: "o1",
        input_choice: null,
        output_choice: null,
        discord_input: "INPUT (2- Volt 1)",
        discord_output: "default",
      },
      self_user: { id: "0", name: "わたし" },
      channel: { id: "c", name: "TRPG卓-1" },
      phase,
      seat_count: n,
      members: [
        { id: "0", name: "わたし", role: gmMode ? "gm" : "pl", presence: "present", grace_secs: null, seat: null, position: null, in_lobby: true, speaking: false, is_self: true, muted: false, volume: 1 } as MemberView,
      ].concat(people.map((p) => {
        const seat = seats.indexOf(p.id);
        return {
          id: p.id,
          name: p.name,
          role: p.role,
          presence: p.presence,
          grace_secs: p.grace,
          seat: seat < 0 ? null : seat,
          in_lobby: p.id !== "5",
          speaking: p.id === "1",
          is_self: false,
          muted: p.id === "2",
          volume: volumes.get(p.id) ?? 1,
          position: seat < 0 ? null : n === 1 ? 0 : -1 + (2 * seat) / (n - 1),
        };
      })),
    };
  };
  const subSnapshot = (): Snapshot => ({
    ...snapshot(),
    side: "sub",
    self_user: { id: "9", name: "わたし（サブ）" },
    channel: { id: "c2", name: "密談" },
    sub: null,
    seat_count: 1,
    members: [
      { id: "9", name: "わたし（サブ）", role: "gm", presence: "present", grace_secs: null, seat: null, position: null, in_lobby: true, speaking: false, is_self: true, muted: false, volume: 1 },
      { id: "7", name: "密談のひと", role: "pl", presence: "present", grace_secs: null, seat: 0, position: fit(0.6), in_lobby: true, speaking: true, is_self: false, muted: false, volume: volumes.get("7") ?? 1 },
    ],
  });
  const full = (): Snapshot => {
    const s = snapshot();
    // メインは左半分（−1〜−g）に寄せる
    const g = 0.2;
    s.members = s.members.map((m) =>
      m.position === null ? m : { ...m, position: fit(dual ? -1 + ((m.position + 1) / 2) * (1 - g) : m.position) },
    );
    return dual ? { ...s, sub: subSnapshot() } : s;
  };
  const emit = () => listener(full());
  // ?still で猶予のカウントダウンを止める（ゴールデン画像の撮影用）
  if (!new URLSearchParams(location.search).has("still")) {
    setInterval(() => {
      const g = people.find((p) => p.presence === "grace");
      if (g && g.grace !== null && g.grace > 0) {
        g.grace--;
        emit();
      }
    }, 1000);
  }

  // スクリーンショット確認用：?click=<CSSセレクタ> で読み込み後にクリックする（点を掴む操作のため pointerdown も送る）
  const click = new URLSearchParams(location.search).get("click");
  if (click)
    setTimeout(() => {
      const el = document.querySelector<HTMLElement>(click);
      el?.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true }));
      el?.click();
    }, 1500);

  return {
    getState: async () => full(),
    onState: (cb) => (listener = cb),
    setPhase: (p) => {
      phase = p;
      emit();
    },
    setRole: (id, role) => {
      people.find((p) => p.id === id)!.role = role;
      if (role === "pl") order = [...order.filter((x) => x !== id), id];
      emit();
    },
    reorder: (id, index) => {
      const seats = order.filter((x) => x !== id && seated(people.find((p) => p.id === x)!.role));
      const rest = order.filter((x) => x !== id && !seats.includes(x));
      seats.splice(index, 0, id);
      order = [...seats, ...rest];
      emit();
    },
    relogin: () => {},
    setGap: () => {},
    setVolume: (id, v) => {
      volumes.set(id, Math.round(Math.max(0, Math.min(2, v)) * 20) / 20);
      emit();
    },
    setSpan: (lo, hi) => {
      span = [lo, hi];
      emit();
    },
    addSubAccount: () => {
      dual = true;
      emit();
    },
    quit: () => {},
    setSelfMute: () => {},
    requestMute: () => {},
    removeSubAccount: () => {
      dual = false;
      emit();
    },
    setInputDevice: () => {},
    setOutputDevice: () => {},
    refreshDevices: () => {},
    openLogDir: () => {},
    setGmMode: (on) => {
      gmMode = on;
      emit();
    },
    resetScenario: () => {
      phase = "before";
      people.forEach((p) => (p.role = "pl"));
      emit();
    },
  };
}
