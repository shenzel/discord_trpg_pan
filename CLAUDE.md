# Discord自動pan

仕様の正は `Discord自動pan 要件定義書.md`。構成とコマンドは `README.md`。

- 方針転換（改訂版）：RPC の pan は音に効かないと実機で判明。声は Social SDK のロビー通話で受け、アプリ内ミキサーで pan する。RPC は VC 検知・ミュート・音量 0 に使う
- 状況：1チャンネルモード（実機確認済み）・同期（GM のメンバーメタデータ、サーバーレス）・2垢モード（1 つのアプリでメインとサブの制御を動かし、`controller::DualShared` で役職と席の数を共有）まで実装済み
- 配布のたびに version を上げる（tauri.conf.json・Cargo.toml・package.json）。インストーラーは上書き更新になる
- Social SDK の呼び出しは `app/src-tauri/native/voice_bridge.cpp` の専用スレッドに集める。Rust からは `voice::Voice` トレイト越しに使い、テストでは `tests/common` の `FakeVoice` に差し替える
- 配置ロジックは `app/src-tauri/src/session.rs` に集め、I/O を持たせない。変更したら単体テストを足す
- RPC の挙動は `app/src-tauri/tests/common/mod.rs` の偽Discordで再現して結合テストする。実機確認が必要なのは、本物のDiscordの応答と聞こえ方だけにする
- 画面を変えたら `npm run test:ui`（モックのゴールデン画像と比べる）。意図した変更なら `test:ui:update` で撮り直し、差分の画像を見てから更新する
- client_secret は配布ビルドに入れない（実行時の環境変数 `DPAN_CLIENT_SECRET` だけ）
- 要件定義書の未決事項は勝手に決めない。切り替えられる形で実装し、ユーザーに確認する
- ユーザーへの確認は図で見せる。C++/Win32 の基礎説明は不要
