import { createApi } from "./api";
import { icons } from "./icons";
import { ellipsize, escapeHtml } from "./text";
import type { Api, Device, MemberView, Phase, Role, Snapshot } from "./types";

const PHASES: { id: Phase; label: string; icon: string }[] = [
  { id: "before", label: "シナリオ前", icon: icons.hourglass },
  { id: "during", label: "シナリオ中", icon: icons.dice },
  { id: "after", label: "感想戦", icon: icons.message },
];

const ROLES: { id: Role; label: string }[] = [
  { id: "pl", label: "PL" },
  { id: "gm", label: "GM" },
  { id: "spectator", label: "観戦者" },
];

const $ = <T extends HTMLElement = HTMLElement>(id: string) => document.getElementById(id) as T;

let api: Api;
let state: Snapshot | null = null;
let dragging: string | null = null;
/** L・R の端をドラッグ中（並ぶ幅は手元の値を見せる） */
let draggingEnd = false;

// ---------- ポップオーバー ----------

const popover = $("popover");
let popoverAnchor: HTMLElement | null = null;

type Placement = "below-center" | "below-right" | "above-right";

function openPopover(anchor: HTMLElement, html: string, placement: Placement, onAction: (action: string) => void) {
  if (popoverAnchor === anchor) return closePopover();
  popoverAnchor = anchor;
  popover.innerHTML = html;
  popover.hidden = false;
  popover.dataset.placement = placement;
  popover.onclick = (e) => {
    const btn = (e.target as HTMLElement).closest<HTMLElement>("[data-action]");
    if (!btn) return;
    closePopover();
    onAction(btn.dataset.action!);
  };
  placePopover();
}

function placePopover() {
  const anchor = popoverAnchor;
  if (!anchor) return;
  // 一覧の再描画でアンカーが作り直されたら閉じる
  if (!anchor.isConnected) return closePopover();
  const placement = popover.dataset.placement as Placement;
  const a = anchor.getBoundingClientRect();
  const p = popover.getBoundingClientRect();
  const margin = 8;
  let left = placement === "below-center" ? a.left + a.width / 2 - p.width / 2 : a.right - p.width;
  left = Math.max(margin, Math.min(left, window.innerWidth - p.width - margin));
  const top = placement === "above-right" ? a.top - p.height - 6 : a.bottom + 6;
  popover.style.left = `${left + window.scrollX}px`;
  popover.style.top = `${top + window.scrollY}px`;
  // 矢印をアンカーの中央に合わせる
  popover.style.setProperty("--arrow-x", `${a.left + a.width / 2 - left}px`);
}

function closePopover() {
  popover.hidden = true;
  popoverAnchor = null;
}

document.addEventListener("pointerdown", (e) => {
  if (popover.hidden) return;
  const t = e.target as Node;
  if (!popover.contains(t) && !popoverAnchor?.contains(t)) closePopover();
});
document.addEventListener("keydown", (e) => e.key === "Escape" && closePopover());
window.addEventListener("resize", placePopover);

function confirmHtml(message: string, ok: string) {
  return `<p class="pop-msg">${message}</p>
    <div class="pop-buttons">
      <button class="btn secondary" data-action="cancel">やめる</button>
      <button class="btn primary" data-action="ok">${ok}</button>
    </div>`;
}

// ---------- 描画 ----------

function render() {
  if (!state) return;
  renderStatus(state);
  renderNotices(state);
  renderSettings(state);
  renderMute(state);
  renderPhases(state);
  renderFooter(state);
  if (!dragging) renderBar(state);
  renderList(state);
  placePopover();
}

function renderStatus(s: Snapshot) {
  const el = $("status");
  let cls = "";
  let html: string;
  switch (s.status.kind) {
    case "setup":
      cls = "error";
      html = "client_id が設定されていません（README を参照）";
      break;
    case "waiting":
      cls = "warn";
      html = `${icons.plugX}<span>Discord の起動を待っています</span>`;
      break;
    case "authorizing":
      cls = "warn";
      html = "Discord の画面で「認証」を押してください";
      break;
    case "error":
      // 全文はお知らせ欄に出す（見出しは短く）
      cls = "error";
      html = `<span>Discord 本体につながっていません</span>`;
      break;
    case "connected":
      html = s.channel
        ? `<span class="vc-name" title="${escapeHtml(s.channel.name)}">${escapeHtml(s.channel.name)}</span>`
        : `<span class="muted">VC に参加していません</span>`;
      if (s.voice.kind === "in_call") html += `<span class="chip ok" title="ロビー通話で左右に振り分けています">振り分け中</span>`;
      if (s.self_user) html += `<span class="self muted">${escapeHtml(ellipsize(s.self_user.name, 8))}</span>`;
      break;
  }
  el.className = `status ${cls}`;
  el.innerHTML = html;
}

function renderPhases(s: Snapshot) {
  const el = $("phases");
  // 確認ポップアップのアンカーを保つため、作るのは一度だけ。以後はクラスだけ切り替える
  if (!el.childElementCount) {
    el.innerHTML = PHASES.map(
      (p, i) => `${i ? '<span class="phase-line"></span>' : ""}
        <button class="phase" data-phase="${p.id}" aria-label="${p.label}">
          <span class="phase-icon">${p.icon}</span>
          <span class="phase-label">${p.label}</span>
        </button>`,
    ).join("");
  }
  for (const btn of el.querySelectorAll<HTMLElement>("[data-phase]")) {
    btn.classList.toggle("active", btn.dataset.phase === s.phase);
  }
  el.classList.toggle("readonly", s.read_only);
}

function renderNotices(s: Snapshot) {
  const items: { cls: string; text: string }[] = [];
  if (s.status.kind === "connected" && s.channel) {
    switch (s.voice.kind) {
      case "starting":
        items.push({ cls: "muted", text: "ロビー通話を準備しています" });
        break;
      case "authorizing":
        items.push({ cls: "warn", text: "ブラウザか Discord の画面で「認証」を押してください" });
        break;
      case "joining":
        items.push({ cls: "muted", text: "ロビー通話につないでいます…" });
        break;
      case "error":
        items.push({ cls: "error", text: `${s.voice.message}。普段の Discord の通話に戻しています` });
        break;
    }
  }
  if (s.status.kind === "error") items.unshift({ cls: "error", text: s.status.message });
  if (s.read_only && s.remote_gm) {
    items.push({ cls: "info", text: `GM「${s.remote_gm.name}」の卓に参加中：役職とフェーズは GM が決めます` });
  }
  for (const w of s.warnings) items.push({ cls: "warn", text: w });
  // ログインのやり直し：Discord 本体にログインできない・アカウントが違う・ロビー通話の許可で失敗したとき
  // 許可を待っている間は出さない（続けて押すと手続きが重なる）
  const needsRelogin = (x: Snapshot) =>
    x.voice.kind !== "authorizing" &&
    (x.status.kind === "error" || x.voice.kind === "error" || x.warnings.some((w) => w.includes("アカウントが違います")));
  const sub = s.sub;
  if (sub) {
    if (sub.status.kind === "error") items.push({ cls: "error", text: `サブ：${sub.status.message}` });
    if (sub.status.kind === "authorizing" || sub.voice.kind === "authorizing")
      items.push({ cls: "warn", text: "サブ：ブラウザか Discord の画面で、サブ垢として「認証」を押してください" });
    if (sub.voice.kind === "error") items.push({ cls: "error", text: `サブ：${sub.voice.message}` });
    for (const w of sub.warnings) if (!s.warnings.includes(w)) items.push({ cls: "warn", text: `サブ：${w}` });
  }
  $("notices").innerHTML =
    items.map((i) => `<p class="notice ${i.cls}">${escapeHtml(i.text)}</p>`).join("") +
    (needsRelogin(s) ? '<button class="btn secondary relogin" data-side="main">ログインし直す</button>' : "") +
    (sub && needsRelogin(sub) ? '<button class="btn secondary relogin" data-side="sub">サブ垢でログインし直す</button>' : "");
}

$("notices").addEventListener("click", (e) => {
  const b = (e.target as HTMLElement).closest<HTMLElement>(".relogin");
  if (b) api.relogin(b.dataset.side === "sub" ? "sub" : "main");
});

// ---------- 設定（マイク・スピーカー） ----------

let settingsOpen = false;
$("settings-toggle").innerHTML = icons.gear;
$("mute-toggle").addEventListener("click", () => state && api.setSelfMute(!state.self_muted));
$("quit").innerHTML = icons.power;
$("quit").addEventListener("click", (e) => {
  openPopover(
    e.currentTarget as HTMLElement,
    confirmHtml("アプリを終了しますか？（Discord の音量とマイクを元に戻します）", "終了する"),
    "below-right",
    (a) => a === "ok" && api.quit(),
  );
});
$("settings-toggle").addEventListener("click", () => {
  settingsOpen = !settingsOpen;
  if (state) render();
});

function deviceSelect(id: string, label: string, list: Device[], choice: string | null, current: string, discord: string | null) {
  const auto = `Discord と同じ（自動）${discord ? `：${escapeHtml(discord)}` : ""}`;
  const options = [`<option value="">${auto}</option>`]
    .concat(
      list.map(
        (d) => `<option value="${escapeHtml(d.id)}" ${choice === d.id ? "selected" : ""}>${escapeHtml(d.name)}</option>`,
      ),
    )
    .join("");
  const using = list.find((d) => d.id === current)?.name ?? "（不明）";
  return `<label class="field">
      <span class="field-label">${label}</span>
      <select id="${id}">${options}</select>
      <span class="field-note">使用中：${escapeHtml(using)}</span>
    </label>`;
}

function renderMute(s: Snapshot) {
  const b = $("mute-toggle");
  b.innerHTML = s.self_muted ? icons.micOff : icons.mic;
  b.classList.toggle("muted", s.self_muted);
  b.title = s.self_muted ? "ミュート中（押すと解除）" : "押すとミュート";
  b.hidden = s.status.kind !== "connected";
}

function renderSettings(s: Snapshot) {
  const el = $("settings");
  el.hidden = !settingsOpen;
  $("settings-toggle").classList.toggle("on", settingsOpen);
  if (!settingsOpen) return;
  // 選択中に作り直すと選べなくなるので、開いている select があれば作り直さない
  if (el.contains(document.activeElement) && ["SELECT", "INPUT"].includes(document.activeElement?.tagName ?? "")) return;
  const a = s.audio;
  el.innerHTML = `
    <h2>ロビー通話のマイクとスピーカー</h2>
    ${deviceSelect("input-device", "マイク", a.inputs, a.input_choice, a.current_input, a.discord_input)}
    ${deviceSelect("output-device", "スピーカー", a.outputs, a.output_choice, a.current_output, a.discord_output)}
    <p class="field-note">通常は「Discord と同じ」のままで大丈夫です。声が聞こえない・届かないときに変えてみてください。</p>
    <div class="settings-buttons">
      <button class="btn secondary" id="refresh-devices">一覧を更新</button>
      <button class="btn secondary" id="open-logs">ログのフォルダを開く</button>
    </div>
    <h2>2垢モード（GM 用）</h2>
    ${
      s.sub
        ? `<p class="field-note">サブ垢：${escapeHtml(s.sub.self_user?.name ?? "ログイン待ち")}（${escapeHtml(
            s.sub.channel?.name ?? "VC なし",
          )}）</p>
          <label class="field">
            <span class="field-label">中央の隙間の幅：<b id="gap-value">${s.gap.toFixed(2)}</b></span>
            <input type="range" id="gap" min="0" max="0.5" step="0.05" value="${s.gap}" />
            <span class="field-note">メインの VC は左（−1〜−幅）、サブの VC は右（＋幅〜＋1）に並べます</span>
          </label>
          <div class="settings-buttons"><button class="btn secondary" id="remove-sub">サブ垢を外す</button></div>`
        : `<p class="field-note">メイン垢とは別の Discord（PTB など）にサブ垢でログインし、サブの VC に入れてから追加してください。許可の画面では、サブ垢であることを確かめてから「認証」を押します。</p>
          <div class="settings-buttons"><button class="btn secondary" id="add-sub">サブ垢を追加</button></div>`
    }`;
}

$("settings").addEventListener("change", (e) => {
  const t = e.target as HTMLSelectElement;
  const id = t.value || null;
  if (t.id === "input-device") api.setInputDevice(id);
  if (t.id === "output-device") api.setOutputDevice(id);
});
$("settings").addEventListener("click", (e) => {
  const t = (e.target as HTMLElement).closest("button");
  if (t?.id === "refresh-devices") api.refreshDevices();
  if (t?.id === "open-logs") api.openLogDir();
  if (t?.id === "add-sub") api.addSubAccount();
  if (t?.id === "remove-sub") api.removeSubAccount();
});
$("settings").addEventListener("input", (e) => {
  const t = e.target as HTMLInputElement;
  if (t.id === "gap") $("gap-value").textContent = Number(t.value).toFixed(2);
});
$("settings").addEventListener("change", (e) => {
  const t = e.target as HTMLInputElement;
  if (t.id === "gap") api.setGap(Number(t.value));
});

function renderFooter(s: Snapshot) {
  const toggle = $<HTMLInputElement>("gm-toggle");
  toggle.checked = s.gm_mode;
  toggle.disabled = s.read_only;
  toggle.parentElement!.title = s.read_only ? "ほかの人が GM として卓を開いています" : "役職とフェーズを、同じ VC のアプリ利用者に配ります";
  $("reset").hidden = s.read_only;
}

$<HTMLInputElement>("gm-toggle").addEventListener("change", (e) => {
  api.setGmMode((e.target as HTMLInputElement).checked);
});

$("phases").addEventListener("click", (e) => {
  const btn = (e.target as HTMLElement).closest<HTMLElement>("[data-phase]");
  if (!btn || !state || state.read_only) return;
  const phase = btn.dataset.phase as Phase;
  if (phase === state.phase) return;
  const label = PHASES.find((p) => p.id === phase)!.label;
  openPopover(btn, confirmHtml(`「${label}」に移動しますか？`, "移動する"), "below-center", (a) => {
    if (a === "ok") api.setPhase(phase);
  });
});

const dots = new Map<string, HTMLElement>();

function renderBar(s: Snapshot) {
  const inner = $("bar-inner");
  const note = $("bar-note");
  const ring = $("center-ring");
  const live = s.status.kind === "connected" && s.channel;

  note.textContent = !live ? "" : s.phase === "before" ? "シナリオ前：pan は初期値のまま" : "";
  // 2垢モードでは、サブ側の人も同じバーに並べる（位置はすでに左右に分かれている）
  const sides: [Snapshot, "main" | "sub"][] = [[s, "main"]];
  if (s.sub && s.sub.status.kind === "connected" && s.sub.channel) sides.push([s.sub, "sub"]);
  const seated: (MemberView & { side: "main" | "sub"; snap: Snapshot })[] =
    live && s.phase !== "before"
      ? sides.flatMap(([x, side]) => x.members.filter((m) => m.position !== null).map((m) => ({ ...m, side, snap: x })))
      : [];
  $("bar").classList.toggle("dual", !!s.sub);
  if (!draggingEnd) showSpan(s.span);

  for (const [id, dot] of dots) {
    if (!seated.some((m) => m.id === id)) {
      dot.remove();
      dots.delete(id);
    }
  }
  for (const m of seated) {
    let dot = dots.get(m.id);
    if (!dot) {
      dot = document.createElement("div");
      dot.className = "dot";
      dot.dataset.id = m.id;
      dot.innerHTML = '<span class="dot-mark"></span><span class="dot-label"></span>';
      dot.addEventListener("pointerdown", startDrag);
      inner.appendChild(dot);
      dots.set(m.id, dot);
    }
    dot.style.left = `${((m.position! + 1) / 2) * 100}%`;
    dot.classList.toggle("absent", m.presence !== "present");
    dot.classList.toggle("speaking", m.speaking);
    dot.classList.toggle("no-app", m.snap.voice.kind === "in_call" && !m.in_lobby && m.presence === "present");
    dot.classList.toggle("main-side", !!s.sub && m.side === "main");
    dot.classList.toggle("sub-side", m.side === "sub");
    dot.dataset.side = m.side;
    dot.classList.toggle("below", (m.seat ?? 0) % 2 === 1);
    const label = dot.querySelector<HTMLElement>(".dot-label")!;
    label.textContent = ellipsize(m.name, 5);
    dot.title = m.name;
  }
  // 重なった点は名前も重なって読めないので、先頭の点に「N人」とまとめる（掴むと誰を動かすか選べる）
  const px = (m: MemberView) => ((m.position! + 1) / 2) * inner.clientWidth;
  const sorted = [...seated].sort((a, b) => a.position! - b.position!);
  for (let i = 0; i < sorted.length; ) {
    let j = i + 1;
    while (j < sorted.length && px(sorted[j]) - px(sorted[j - 1]) < OVERLAP_PX) j++;
    if (j - i > 1) {
      sorted.slice(i, j).forEach((m, k) => {
        const dot = dots.get(m.id)!;
        dot.classList.toggle("below", false);
        dot.querySelector<HTMLElement>(".dot-label")!.textContent = k === 0 ? `${j - i}人` : "";
      });
    }
    i = j;
  }

  const centered = live && s.phase !== "before"
    ? s.members.filter((m) => m.position === null && m.presence === "present" && !m.is_self)
    : [];
  ring.hidden = !live || s.phase === "before";
  $("ring-count").textContent = centered.length ? `${centered.length}人` : "";
  ring.title = centered.length ? `中央から鳴る人：${centered.map((m) => m.name).join("、")}` : "席なしで中央から鳴る人はいません";
}

/** 点を掴んだとき：重なった点なら誰を動かすか選ぶ。1 人ならそのまま並べ替える。 */
function startDrag(e: PointerEvent) {
  const dot = e.currentTarget as HTMLElement;
  // 名前を「N人」とまとめたのと同じく、間隔が OVERLAP_PX 未満で連なった点をひとかたまりにする
  const sorted = [...dots.values()].sort((a, b) => a.getBoundingClientRect().left - b.getBoundingClientRect().left);
  const groups: HTMLElement[][] = [];
  sorted.forEach((d, i) => {
    const gap = i ? d.getBoundingClientRect().left - sorted[i - 1].getBoundingClientRect().left : Infinity;
    if (gap < OVERLAP_PX) groups[groups.length - 1].push(d);
    else groups.push([d]);
  });
  const overlapping = groups.find((g) => g.includes(dot))!;
  if (overlapping.length < 2) return dragDot(dot, e);
  const items = overlapping
    .map((d) => `<button class="menu-item pick ${d.classList.contains("main-side") ? "main-side" : ""} ${d.classList.contains("sub-side") ? "sub-side" : ""}" data-pick="${escapeHtml(d.dataset.id!)}">
        <span class="check"><span class="pick-mark"></span></span>${escapeHtml(ellipsize(d.title, 10))}</button>`)
    .join("");
  openPopover(dot, `<div class="menu">${items}<p class="menu-note pick-note">名前を掴んで左右へドラッグ</p></div>`, "below-center", () => {});
}

/** これより近い点は重なっているとみなす（点の直径くらい） */
const OVERLAP_PX = 12;

// 重なった点のメニューで名前を掴むと、その人の点をドラッグする
popover.addEventListener("pointerdown", (e) => {
  const item = (e.target as HTMLElement).closest<HTMLElement>("[data-pick]");
  const dot = item && dots.get(item.dataset.pick!);
  if (!dot) return;
  e.preventDefault();
  closePopover();
  dragDot(dot, e);
});

function dragDot(dot: HTMLElement, e: PointerEvent) {
  const own = dot.dataset.side === "sub" ? state?.sub : state;
  if (!state || !own || own.seat_count < 2) return;
  const id = dot.dataset.id!;
  // その人の側の席の位置（並べ替えは同じ VC の中だけ）
  const seats = own.members.filter((m) => m.position !== null).sort((a, b) => (a.seat ?? 0) - (b.seat ?? 0));
  const inner = $("bar-inner");
  dragging = id;
  dot.setPointerCapture(e.pointerId);
  dot.classList.add("dragging");
  const x0 = e.clientX;
  let moved = false;
  let frac = 0;
  const move = (ev: PointerEvent) => {
    // 動かさずに離したときは並べ替えない（重なった点を選んだだけのとき）
    moved ||= Math.abs(ev.clientX - x0) > 3;
    if (!moved) return;
    const r = inner.getBoundingClientRect();
    frac = Math.max(0, Math.min(1, (ev.clientX - r.left) / r.width));
    dot.style.left = `${frac * 100}%`;
  };
  const up = () => {
    dot.removeEventListener("pointermove", move);
    dot.removeEventListener("pointerup", up);
    dot.removeEventListener("pointercancel", up);
    dot.classList.remove("dragging");
    dragging = null;
    if (moved) {
      const p = frac * 2 - 1;
      let index = 0;
      seats.forEach((m, i) => {
        if (Math.abs(m.position! - p) < Math.abs(seats[index].position! - p)) index = i;
      });
      api.reorder(id, index);
    }
    render();
  };
  dot.addEventListener("pointermove", move);
  dot.addEventListener("pointerup", up);
  dot.addEventListener("pointercancel", up);
}

/** 席を並べる範囲の L 端と R 端（ドラッグ中は手元の値） */
let span: [number, number] = [-1, 1];

function setSpan(lo: number, hi: number) {
  const round = (v: number) => Math.round(v * 20) / 20;
  lo = Math.max(-1, Math.min(round(lo), span[1]));
  hi = Math.max(lo, Math.min(round(hi), 1));
  if (lo === span[0] && hi === span[1]) return;
  showSpan([lo, hi]);
  api.setSpan(lo, hi);
}

function showSpan(next: [number, number]) {
  span = next;
  const inner = $("bar-inner");
  inner.style.setProperty("--lo", String(span[0]));
  inner.style.setProperty("--hi", String(span[1]));
}

// L・R の端を内側へ寄せると、席の並ぶ範囲が縮む（人と人の間隔が狭まる）。L と R は別々に動く
for (const end of document.querySelectorAll<HTMLElement>(".bar-end")) {
  const isLeft = end.dataset.end === "left";
  end.addEventListener("pointerdown", (e) => {
    const inner = $("bar-inner");
    draggingEnd = true;
    end.setPointerCapture(e.pointerId);
    end.classList.add("dragging");
    // 掴んだ位置と端の席とのずれを保ったまま動かす
    const r0 = inner.getBoundingClientRect();
    const offset = e.clientX - (r0.left + ((span[isLeft ? 0 : 1] + 1) / 2) * r0.width);
    const move = (ev: PointerEvent) => {
      const r = inner.getBoundingClientRect();
      const p = ((ev.clientX - offset - r.left) / r.width) * 2 - 1;
      if (isLeft) setSpan(p, span[1]);
      else setSpan(span[0], p);
    };
    const up = () => {
      end.removeEventListener("pointermove", move);
      end.removeEventListener("pointerup", up);
      end.removeEventListener("pointercancel", up);
      end.classList.remove("dragging");
      draggingEnd = false;
      render();
    };
    end.addEventListener("pointermove", move);
    end.addEventListener("pointerup", up);
    end.addEventListener("pointercancel", up);
  });
  end.addEventListener("dblclick", () => (isLeft ? setSpan(-1, span[1]) : setSpan(span[0], 1)));
}

function formatSecs(s: number) {
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}

/** 誰がロビー通話でつながっているか（左右に振り分けられるか）。 */
function connection(m: MemberView, s: Snapshot): { cls: string; title: string } {
  if (m.presence !== "present") return { cls: "off", title: "VC にいません" };
  if (m.is_self) {
    if (s.voice.kind === "in_call") return { cls: "on", title: "ロビー通話につながっています" };
    if (s.voice.kind === "error") return { cls: "err", title: "ロビー通話に入れません" };
    return { cls: "wait", title: "ロビー通話につないでいます" };
  }
  if (m.in_lobby) return { cls: "on", title: "ロビー通話でつながっています（左右に振り分け）" };
  if (s.voice.kind === "in_call") return { cls: "noapp", title: "アプリなし：Discord 経由で中央から聞こえます" };
  return { cls: "wait", title: "自分がロビー通話につながっていません" };
}

function rowHtml(m: MemberView, s: Snapshot) {
  let badge = "";
  if (m.presence === "grace" && m.grace_secs !== null) {
    badge = `<span class="badge warn" title="回線落ち：猶予の残り時間">${icons.plugX}${formatSecs(m.grace_secs)}</span>`;
  } else if (m.presence === "away") {
    badge = `<span class="badge muted">不在</span>`;
  } else if (s.voice.kind === "in_call" && !m.in_lobby) {
    badge = `<span class="badge muted" title="このアプリを使っていないので、Discord 経由で中央から聞こえます">アプリなし</span>`;
  }
  const name = escapeHtml(m.name);
  const more = s.read_only
    ? ""
    : `<button class="more" data-menu="${escapeHtml(m.id)}" aria-label="役職を変更">${icons.dots}</button>`;
  const conn = connection(m, s);
  const mute = m.muted && m.presence === "present" ? `<span class="row-mute" title="ミュート中">${icons.micOff}</span>` : "";
  return `<li class="row ${m.presence !== "present" ? "absent" : ""} ${m.is_self ? "self-row" : ""}">
      <span class="conn ${conn.cls}" title="${conn.title}"></span>
      <span class="name ${m.speaking ? "speaking" : ""}" title="${name}">${escapeHtml(ellipsize(m.name, 8))}${m.is_self ? '<span class="you">（自分）</span>' : ""}</span>
      ${mute}
      ${badge}
      ${more}
    </li>`;
}

function renderList(s: Snapshot) {
  const el = $("list");
  if (s.status.kind !== "connected" || !s.channel) {
    el.innerHTML = "";
    return;
  }
  if (s.members.filter((m) => !m.is_self).length === 0 && s.members.length === 0) {
    el.innerHTML = '<p class="empty muted">VC にほかの参加者はいません</p>';
    return;
  }
  const groups = (x: Snapshot) =>
    ROLES.map((r) => {
      const ms = x.members.filter((m) => m.role === r.id);
      return `<div class="group">
        <h2>${r.label}<span class="count">${ms.length}</span></h2>
        <ul>${ms.map((m) => rowHtml(m, x)).join("") || '<li class="none muted">なし</li>'}</ul>
      </div>`;
    }).join("");
  const legend = `<p class="legend"><span class="conn on"></span>つながっている（左右に振り分け）<span class="conn noapp"></span>アプリなし（中央）</p>`;
  el.classList.toggle("dual", !!s.sub);
  if (s.sub) {
    const head = (x: Snapshot, label: string) =>
      `<h3 class="col-head ${x.side}" title="${escapeHtml(x.channel?.name ?? "")}">${label}：${escapeHtml(
        ellipsize(x.self_user?.name ?? "（ログイン待ち）", 6),
      )}<span class="col-vc">${escapeHtml(ellipsize(x.channel?.name ?? "VC なし", 6))}</span></h3>`;
    el.innerHTML =
      `<div class="cols"><div class="col">${head(s, "メイン")}${groups(s)}</div>` +
      `<div class="col">${head(s.sub, "サブ")}${s.sub.channel ? groups(s.sub) : '<p class="none muted">サブ垢が VC にいません</p>'}</div></div>` +
      legend;
  } else {
    el.innerHTML = groups(s) + legend;
  }
  // 開いている「⋯」メニューは、作り直した同じ人のボタンに付け替える
  const menuId = popoverAnchor?.dataset.menu;
  if (menuId) {
    const next = [...el.querySelectorAll<HTMLElement>("[data-menu]")].find((b) => b.dataset.menu === menuId);
    if (next) popoverAnchor = next;
  }
}

$("list").addEventListener("click", (e) => {
  const btn = (e.target as HTMLElement).closest<HTMLElement>("[data-menu]");
  if (!btn || !state) return;
  const id = btn.dataset.menu!;
  const m = state.members.concat(state.sub?.members ?? []).find((x) => x.id === id);
  if (!m) return;
  const owner = state.sub?.members.some((x) => x.id === id) ? state.sub : state;
  const canMute = m.is_self || (owner.gm_mode && m.presence === "present");
  const muteItem = canMute
    ? `<div class="menu-sep"></div><button class="menu-item" data-action="${m.muted ? "unmute" : "mute"}">
        <span class="check">${m.muted ? icons.mic : icons.micOff}</span>${m.muted ? "ミュートを解除" : "ミュートにする"}${
          m.is_self ? "" : '<span class="menu-note">（本人が解除できます）</span>'
        }</button>`
    : "";
  const html = `<div class="menu">${ROLES.map(
    (r) => `<button class="menu-item ${r.id === m.role ? "current" : ""}" data-action="${r.id}">
        <span class="check">${r.id === m.role ? icons.check : ""}</span>${r.label}</button>`,
  ).join("")}${muteItem}</div>`;
  openPopover(btn, html, "below-right", (action) => {
    if (action === "mute" || action === "unmute") {
      if (m.is_self) api.setSelfMute(action === "mute");
      else api.requestMute(id, action === "mute");
    } else if (action !== m.role) api.setRole(id, action as Role);
  });
});

$("reset").addEventListener("click", (e) => {
  openPopover(
    e.currentTarget as HTMLElement,
    confirmHtml("pan を初期値に戻し、役職の割り当ても消します。", "リセットする"),
    "above-right",
    (a) => a === "ok" && api.resetScenario(),
  );
});

// ---------- 起動 ----------

createApi().then(async (a) => {
  api = a;
  api.onState((s) => {
    state = s;
    render();
  });
  state = await api.getState();
  render();
});
