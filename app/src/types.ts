export type Role = "pl" | "gm" | "spectator";
export type Phase = "before" | "during" | "after";

export type Status =
  | { kind: "setup" }
  | { kind: "waiting" }
  | { kind: "authorizing" }
  | { kind: "error"; message: string }
  | { kind: "connected" };

/** ロビー通話の状態 */
export type VoiceStatus =
  | { kind: "starting" }
  | { kind: "authorizing" }
  | { kind: "idle" }
  | { kind: "joining" }
  | { kind: "in_call" }
  | { kind: "error"; message: string };

export interface MemberView {
  id: string;
  name: string;
  role: Role;
  presence: "present" | "grace" | "away";
  grace_secs: number | null;
  seat: number | null;
  position: number | null;
  /** このアプリでロビー通話に入っている（左右に振れる） */
  in_lobby: boolean;
  speaking: boolean;
  /** 自分の行（席は持たない） */
  is_self: boolean;
  /** Discord でミュート中 */
  muted: boolean;
}

export interface Device {
  id: string;
  name: string;
  default: boolean;
}

export interface AudioView {
  inputs: Device[];
  outputs: Device[];
  current_input: string;
  current_output: string;
  /** 指定しているデバイス（null は Discord と同じ） */
  input_choice: string | null;
  output_choice: string | null;
  discord_input: string | null;
  discord_output: string | null;
}

export interface Snapshot {
  status: Status;
  voice: VoiceStatus;
  self_user: { id: string; name: string } | null;
  channel: { id: string; name: string } | null;
  phase: Phase;
  members: MemberView[];
  seat_count: number;
  gm_mode: boolean;
  remote_gm: { id: string; name: string } | null;
  read_only: boolean;
  warnings: string[];
  audio: AudioView;
  side: "main" | "sub";
  self_muted: boolean;
  /** 2垢モードの隙間の半幅 g */
  gap: number;
  /** 2垢モードのサブ側 */
  sub: Snapshot | null;
}

export interface Api {
  getState(): Promise<Snapshot>;
  onState(cb: (s: Snapshot) => void): void;
  setPhase(phase: Phase): void;
  setRole(userId: string, role: Role): void;
  reorder(userId: string, index: number): void;
  resetScenario(): void;
  setGmMode(on: boolean): void;
  relogin(side?: "main" | "sub"): void;
  setGap(gap: number): void;
  addSubAccount(): void;
  removeSubAccount(): void;
  quit(): void;
  setSelfMute(on: boolean): void;
  requestMute(userId: string, on: boolean): void;
  setInputDevice(id: string | null): void;
  setOutputDevice(id: string | null): void;
  refreshDevices(): void;
  openLogDir(): void;
}
