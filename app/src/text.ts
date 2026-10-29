/** 半角=1、全角=2 で数えた表示幅。 */
export function displayWidth(s: string): number {
  let w = 0;
  for (const ch of s) w += charWidth(ch);
  return w;
}

function charWidth(ch: string): number {
  const c = ch.codePointAt(0)!;
  return c <= 0x7e || (c >= 0xff61 && c <= 0xff9f) ? 1 : 2;
}

/** 全角 maxFull 文字ぶんの幅を超えたら「…」で省略する。 */
export function ellipsize(s: string, maxFull: number): string {
  const max = maxFull * 2;
  if (displayWidth(s) <= max) return s;
  let out = "";
  let w = 0;
  for (const ch of s) {
    const cw = charWidth(ch);
    if (w + cw > max - 2) break;
    out += ch;
    w += cw;
  }
  return out + "…";
}

export function escapeHtml(s: string): string {
  return s.replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);
}
