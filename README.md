# Discord自動pan

Discord の VC 参加者の声を左右に振り分ける常駐アプリ（TRPG向け）。仕様は [要件定義書](Discord自動pan%20要件定義書.md)。

声は Discord Social SDK のロビー通話で受け取り、アプリ内で話者ごとに左右の倍率を掛ける。Discord 本体（RPC）は、今いる VC の検知と、二重に聞こえないための調整（Discord 側のマイク入力 0・アプリ利用者の音量 0）に使う。

```
app/
  src-tauri/src/   rpc（名前付きパイプ）・auth・pan（等パワー則）・session（役職・フェーズ・並び順）
                   animator（ゆっくり移動）・voice（Social SDK の FFI）・controller（制御ループ）
  src-tauri/native/voice_bridge.cpp   Social SDK を専用スレッドで動かす C++ のブリッジ
  src/             画面（ブラウザで npm run dev するとモックで表示）
probes/lobby-probe/  ロビー通話の実機検証ツール（C++）
third_party/         Social SDK（git には入れない）
```

## 1. Discord Developer Portal の準備

1. アプリを作り、OAuth2 の Client ID を控える（Bot は不要）
2. OAuth2 → Redirects に `http://127.0.0.1/callback` と `http://localhost` を登録する
3. OAuth2 → **Public Client** をオンにする（client_secret なしでログインするため。ログインは Social SDK の PKCE で行い、そのトークンを Discord 本体との連携にも使う）
4. Discord Social SDK → Getting Started のフォームを送信し、Downloads から C++ 用 SDK をダウンロードする
5. **App Testers に、使う人全員の Discord アカウントを追加する**（各自に届くメールで承認してもらう）。Discord 本体との連携（rpc スコープ）は、審査を受けていないアプリでは持ち主とテスターしか許可できないため。最大 50 人
6. SDK を `third_party/sdk-1.10.19337/` に展開する（別の場所なら環境変数 `DPAN_SDK_DIR` で `discord_social_sdk` フォルダを指す）

## 2. ビルドと起動

前提：Rust、Node.js、Visual Studio 2022 の C++ ビルドツール。

```powershell
cd app
npm install
npm run tauri dev      # 開発（.env を読む）
npx tauri build        # 配布用インストーラー → src-tauri/target/release/bundle/nsis/
                       # 配るたびに tauri.conf.json・Cargo.toml・package.json の version を上げる（入っている版を上書き更新する）
```

client_id は、リポジトリ直下の `.env`（`DPAN_CLIENT_ID=...`、開発ビルドのみ）、ビルド時の環境変数 `DPAN_CLIENT_ID`、`%APPDATA%\com.shenzel.discordautopan\config.json`（`{"client_id": "..."}`）のどれかで与える。配布するときはビルド時の環境変数で埋め込む。

## 3. 使い方

- PL：アプリを起動して、Discord で VC に入るだけ。初回だけ、ブラウザか Discord の画面で 1 回許可する（Discord 本体と同じアカウントで）
- GM：画面左下の「GM として卓を開く」をオンにすると、役職とフェーズが同じ VC のアプリ利用者に配られる
- 2垢モード（GM）：別の Discord（PTB など）にサブ垢でログインしてサブの VC に入れ、設定の「サブ垢を追加」でサブ垢として許可する。メインの VC は左、サブの VC は右に並ぶ（隙間の幅は設定で変えられる）。GM の声は両方の VC に届く
- 並ぶ範囲：L-R バーの L・R を別々に内側へドラッグすると、人と人の間隔が狭まる（ダブルクリックで元に戻す）。重なった点は掴むと誰を動かすか選べる
- 人ごとの音量：一覧の「⋯」メニューで 0〜200%
- アプリはロビー通話と同じアカウントでログインしている Discord につなぐ（Stable と PTB を同時に起動していても取り違えない）
- マイクとスピーカーは Discord で選んでいるものに自動で合わせる。合わないときは右上の歯車（設定）で指定できる
- 一覧の名前の左の印：緑＝ロビー通話でつながっている（左右に振り分け）、灰色の輪＝アプリなし（Discord 経由で中央）
- 不具合の報告：設定の「ログのフォルダを開く」で開く `DiscordAutoPan.log` を送ってもらう（5 秒ごとの音声の状況も記録している）
- アプリを終了すると、Discord 側の音量とマイク入力は自動で元に戻る

## テスト

```powershell
cd app/src-tauri; cargo test     # 単体テスト＋偽 Discord（名前付きパイプ）と偽ロビー通話での結合テスト
cd app; npm run build            # 型チェック＋画面のビルド
cd app; npm run test:ui          # 画面のゴールデン画像テスト（モックを撮って tests-ui/golden と比べる。Chrome を使う）
cd app; npm run test:ui:update   # 画面を意図して変えたとき、基準画像を撮り直す
```

## 実機検証ツール

- `app/src-tauri/src/bin/pan_probe.rs`：RPC の pan と音量の検証（pan は効かないと判明）
- `probes/lobby-probe/`：ロビー通話の検証。`build.bat` でビルドし、`out\lobby-probe.exe` を起動する
