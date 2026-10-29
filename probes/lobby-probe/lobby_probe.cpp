// lobby-probe: Social SDK のロビー通話で、方式の致命的リスクを確かめる実機検証ツール。
//
//   lobby-probe [--secret S] [--rpc-scopes] [--relogin]
//
// 起動するとログイン → ロビー参加 → 通話開始まで進み、以後は標準入力のコマンドで操作する。
// 確かめること:
//   - Discord 本体が VC にいるまま、同じアカウントでロビー通話に入れるか（目視）
//   - 話者ごとの音声が届くか、何チャンネルで届くか、その場で pan を掛けて左右に分かれるか
//   - メンバーメタデータが他の人に届くか、届くまでの時間、他人のメタデータを書けないか
//   - ロビー参加と通話の開始・終了が回数上限に数えられるか（上限時のエラーの形）
//
// client_id は .env の DPAN_CLIENT_ID（カレントか exe から親へ探す）か、exe と同じ場所の client_id.txt から読む。

#define DISCORDPP_IMPLEMENTATION
#include "discordpp.h"

#define NOMINMAX
#include <windows.h>
#include <audioclient.h>
#include <mmdeviceapi.h>
#pragma comment(lib, "ole32.lib")

#include <algorithm>
#include <atomic>
#include <chrono>
#include <cmath>
#include <cstdio>
#include <deque>
#include <filesystem>
#include <fstream>
#include <iostream>
#include <map>
#include <mutex>
#include <sstream>
#include <string>
#include <thread>

#include <share.h>

namespace fs = std::filesystem;
using Clock = std::chrono::steady_clock;

// ---------- 設定 ----------

static std::string Trim(std::string s) {
    auto notSpace = [](unsigned char c) { return !std::isspace(c); };
    s.erase(s.begin(), std::find_if(s.begin(), s.end(), notSpace));
    s.erase(std::find_if(s.rbegin(), s.rend(), notSpace).base(), s.end());
    return s;
}

static fs::path ExeDir() {
    wchar_t buf[MAX_PATH];
    GetModuleFileNameW(nullptr, buf, MAX_PATH);
    return fs::path(buf).parent_path();
}

static std::string FindEnvValue(const fs::path& start, const std::string& key) {
    for (fs::path dir = start; !dir.empty(); dir = dir.parent_path()) {
        std::ifstream f(dir / ".env");
        std::string line;
        while (f && std::getline(f, line)) {
            if (line.rfind(key + "=", 0) == 0) {
                auto v = Trim(line.substr(key.size() + 1));
                if (!v.empty()) return v;
            }
        }
        if (dir == dir.parent_path()) break;
    }
    return {};
}

static uint64_t LoadClientId() {
    std::string id = FindEnvValue(fs::current_path(), "DPAN_CLIENT_ID");
    if (id.empty()) id = FindEnvValue(ExeDir(), "DPAN_CLIENT_ID");
    if (id.empty()) {
        std::ifstream f(ExeDir() / "client_id.txt");
        std::getline(f, id);
        id = Trim(id);
    }
    return id.empty() ? 0 : std::stoull(id);
}

static fs::path TokenPath(const std::string& file) {
    wchar_t* appdata = nullptr;
    size_t len = 0;
    _wdupenv_s(&appdata, &len, L"APPDATA");
    fs::path dir = fs::path(appdata ? appdata : L".") / "com.shenzel.discordautopan";
    free(appdata);
    fs::create_directories(dir);
    return dir / file;
}

struct SavedToken {
    std::string access, refresh, scopes;
    int64_t expiresAt = 0;  // UNIX 秒
};

static int64_t NowSecs() {
    return std::chrono::duration_cast<std::chrono::seconds>(
             std::chrono::system_clock::now().time_since_epoch())
      .count();
}

static std::optional<SavedToken> LoadToken(const std::string& file) {
    std::ifstream f(TokenPath(file));
    SavedToken t;
    std::string exp;
    if (!std::getline(f, t.access) || !std::getline(f, t.refresh) || !std::getline(f, exp)) return {};
    std::getline(f, t.scopes);
    t.expiresAt = std::stoll(exp);
    return t;
}

static void SaveToken(const std::string& file, const SavedToken& t) {
    std::ofstream f(TokenPath(file), std::ios::trunc);
    f << t.access << "\n" << t.refresh << "\n" << t.expiresAt << "\n" << t.scopes << "\n";
}

// ---------- ログ ----------

static auto g_start = Clock::now();

static FILE* g_logFile = nullptr;  // exe と同じ場所の lobby-probe.log にも書く
static std::mutex g_logMutex;

template <class... A>
static void Log(const char* fmt, A... a) {
    std::lock_guard lock(g_logMutex);
    double t = std::chrono::duration<double>(Clock::now() - g_start).count();
    for (FILE* f : {stdout, g_logFile}) {
        if (!f) continue;
        std::fprintf(f, "[%7.2f] ", t);
        std::fprintf(f, fmt, a...);
        std::fprintf(f, "\n");
        std::fflush(f);
    }
}

static std::string Describe(const discordpp::ClientResult& r) {
    std::ostringstream o;
    o << "error=\"" << r.Error() << "\" code=" << r.ErrorCode() << " http=" << static_cast<int>(r.Status())
      << " retryable=" << r.Retryable() << " retryAfter=" << r.RetryAfter() << "s";
    return o.str();
}

// ---------- 音声（ボイススレッドから呼ばれる） ----------

static std::atomic<bool> g_running{true};

struct UserStats {
    uint64_t callbacks = 0, samples = 0;
    uint64_t channels = 0, samplesPerChannel = 0;
    int32_t rate = 0;
    double peak = 0;
};

static std::mutex g_statsMutex;
static std::map<uint64_t, UserStats> g_stats;
static std::atomic<uint64_t> g_capturedCallbacks{0}, g_capturedChannels{0};

enum class PanMode { Off, Fixed, Sweep };
static std::atomic<PanMode> g_panMode{PanMode::Fixed};
static std::atomic<float> g_panL{1.0f}, g_panR{0.0f};

static void CurrentPan(float& l, float& r) {
    switch (g_panMode.load()) {
    case PanMode::Off:
        l = r = 1.0f;
        return;
    case PanMode::Fixed:
        l = g_panL;
        r = g_panR;
        return;
    case PanMode::Sweep: {
        // 4秒で左端→右端→左端。要件定義書と同じ等パワー則（端 0.3）
        double t = std::chrono::duration<double>(Clock::now() - g_start).count();
        double p = std::sin(t * 3.14159265358979 / 2.0);
        double edge = std::atan(0.3), theta = 3.14159265358979 / 4 + p * (3.14159265358979 / 4 - edge);
        l = static_cast<float>(std::cos(theta) / std::cos(edge));
        r = static_cast<float>(std::sin(theta) / std::cos(edge));
        return;
    }
    }
}

// ---------- 自前の再生（モノラルで届いた声用） ----------
//
// モノラルではその場で左右に振れないので、SDK の再生を止め（outShouldMute）、
// 話者ごとのバッファに貯めて WASAPI（共有モード・48kHz・2ch・16bit）で pan を掛けて鳴らす。

constexpr uint32_t kOutRate = 48000;
constexpr size_t kPrebuffer = kOutRate * 20 / 1000;   // 再生を始めるまでに貯める量（20ms）
constexpr size_t kMaxBuffer = kOutRate * 200 / 1000;  // これを超えたら古い分を捨てる（200ms）

struct MonoBuffer {
    std::deque<int16_t> samples;
    bool playing = false;
    uint64_t underruns = 0, drops = 0;
};

static std::mutex g_mixMutex;
static std::map<uint64_t, MonoBuffer> g_mix;
static std::atomic<bool> g_outputRunning{false};
static std::atomic<uint64_t> g_outputFrames{0};
static std::once_flag g_outputOnce;

static void MixInto(int16_t* out, uint32_t frames) {
    float l, r;
    CurrentPan(l, r);
    std::vector<int32_t> acc(frames * 2, 0);
    {
        std::lock_guard lock(g_mixMutex);
        for (auto& [id, b] : g_mix) {
            if (!b.playing) {
                if (b.samples.size() < kPrebuffer) continue;
                b.playing = true;
            }
            uint32_t n = static_cast<uint32_t>(std::min<size_t>(frames, b.samples.size()));
            for (uint32_t i = 0; i < n; ++i) {
                int32_t s = b.samples.front();
                b.samples.pop_front();
                acc[2 * i] += static_cast<int32_t>(s * l);
                acc[2 * i + 1] += static_cast<int32_t>(s * r);
            }
            if (n < frames) {  // 足りなければ、また貯まるまで待つ
                b.playing = false;
                b.underruns++;
            }
        }
    }
    for (uint32_t i = 0; i < frames * 2; ++i) out[i] = static_cast<int16_t>(std::clamp(acc[i], -32768, 32767));
    g_outputFrames += frames;
}

static void OutputThread() {
    auto fail = [](const char* what, HRESULT hr) { Log("自前の再生を開始できません: %s (0x%08lx)", what, hr); };
    CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    IMMDeviceEnumerator* en = nullptr;
    IMMDevice* dev = nullptr;
    IAudioClient* ac = nullptr;
    IAudioRenderClient* rc = nullptr;
    HRESULT hr = CoCreateInstance(__uuidof(MMDeviceEnumerator), nullptr, CLSCTX_ALL, IID_PPV_ARGS(&en));
    if (FAILED(hr)) return fail("MMDeviceEnumerator", hr);
    if (FAILED(hr = en->GetDefaultAudioEndpoint(eRender, eConsole, &dev))) return fail("既定の出力デバイス", hr);
    if (FAILED(hr = dev->Activate(__uuidof(IAudioClient), CLSCTX_ALL, nullptr, reinterpret_cast<void**>(&ac))))
        return fail("IAudioClient", hr);
    WAVEFORMATEX wf{};
    wf.wFormatTag = WAVE_FORMAT_PCM;
    wf.nChannels = 2;
    wf.nSamplesPerSec = kOutRate;
    wf.wBitsPerSample = 16;
    wf.nBlockAlign = 4;
    wf.nAvgBytesPerSec = kOutRate * 4;
    const REFERENCE_TIME bufferDuration = 300000;  // 30ms（100ns 単位）
    hr = ac->Initialize(AUDCLNT_SHAREMODE_SHARED,
                        AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM |
                          AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                        bufferDuration, 0, &wf, nullptr);
    if (FAILED(hr)) return fail("IAudioClient::Initialize", hr);
    HANDLE ev = CreateEventW(nullptr, FALSE, FALSE, nullptr);
    ac->SetEventHandle(ev);
    UINT32 bufferFrames = 0;
    ac->GetBufferSize(&bufferFrames);
    if (FAILED(hr = ac->GetService(IID_PPV_ARGS(&rc)))) return fail("IAudioRenderClient", hr);
    ac->Start();
    g_outputRunning = true;
    Log("自前の再生を開始（WASAPI 共有モード、バッファ %u フレーム = %.1fms）", bufferFrames,
        bufferFrames * 1000.0 / kOutRate);
    while (g_running) {
        WaitForSingleObject(ev, 200);
        UINT32 padding = 0;
        if (FAILED(ac->GetCurrentPadding(&padding))) break;
        UINT32 frames = bufferFrames - padding;
        if (frames == 0) continue;
        BYTE* p = nullptr;
        if (FAILED(rc->GetBuffer(frames, &p))) break;
        MixInto(reinterpret_cast<int16_t*>(p), frames);
        rc->ReleaseBuffer(frames, 0);
    }
    ac->Stop();
    rc->Release();
    ac->Release();
    dev->Release();
    en->Release();
    CloseHandle(ev);
    g_outputRunning = false;
}

static void OnAudioReceived(uint64_t userId, int16_t* data, uint64_t samplesPerChannel, int32_t rate,
                            uint64_t channels, bool& outShouldMute) {
    double peak = 0;
    for (uint64_t i = 0; i < samplesPerChannel * channels; ++i) peak = std::max(peak, std::abs(data[i] / 32768.0));
    {
        std::lock_guard lock(g_statsMutex);
        auto& s = g_stats[userId];
        s.callbacks++;
        s.samples += samplesPerChannel;
        s.channels = channels;
        s.samplesPerChannel = samplesPerChannel;
        s.rate = rate;
        s.peak = std::max(s.peak, peak);
    }
    if (channels == 1) {
        // off のときは SDK にそのまま鳴らさせる（自前の経路との聞き比べ用）
        if (g_panMode == PanMode::Off) return;
        std::call_once(g_outputOnce, [] { std::thread(OutputThread).detach(); });
        if (rate != static_cast<int32_t>(kOutRate)) return;  // 想定外のレートは stats で分かるのでそのまま鳴らす
        outShouldMute = true;
        std::lock_guard lock(g_mixMutex);
        auto& b = g_mix[userId];
        b.samples.insert(b.samples.end(), data, data + samplesPerChannel);
        while (b.samples.size() > kMaxBuffer) {
            b.samples.pop_front();
            b.drops++;
        }
        return;
    }
    if (channels != 2) return;
    float l, r;
    CurrentPan(l, r);
    for (uint64_t i = 0; i < samplesPerChannel; ++i) {
        data[2 * i] = static_cast<int16_t>(std::clamp(data[2 * i] * l, -32768.0f, 32767.0f));
        data[2 * i + 1] = static_cast<int16_t>(std::clamp(data[2 * i + 1] * r, -32768.0f, 32767.0f));
    }
}

static std::atomic<float> g_capturedPeak{0};  // stats を出すたびに 0 に戻す

static void OnAudioCaptured(int16_t* data, uint64_t samplesPerChannel, int32_t, uint64_t channels) {
    g_capturedCallbacks++;
    g_capturedChannels = channels;
    float peak = 0;
    for (uint64_t i = 0; i < samplesPerChannel * channels; ++i) peak = std::max(peak, std::abs(data[i] / 32768.0f));
    if (peak > g_capturedPeak) g_capturedPeak = peak;
}

// ---------- コマンド入力 ----------

static std::mutex g_cmdMutex;
static std::deque<std::string> g_cmds;
static void InputThread() {
    std::string line;
    while (g_running && std::getline(std::cin, line)) {
        std::lock_guard lock(g_cmdMutex);
        g_cmds.push_back(Trim(line));
    }
}

static const char* kHelp = R"(コマンド:
  pan L R      受信音に掛ける左右の倍率（例: pan 1 0 で完全に左）
  sweep        4秒周期で左右に往復（等パワー則）
  off          pan を掛けない
  stats        受信状況を表示（5秒ごとにも自動表示）
  members      ロビーのメンバーとメタデータを表示
  meta K=V     自分のメンバーメタデータを書く（CreateOrJoinLobbyWithMetadata。回数枠を使う可能性あり）
  devices      マイク（入力デバイス）の一覧と、SDK が使っているものを表示
  input N      devices の N 番のマイクに切り替える
  unmute       自分のミュートを解除する
  vad [dB|auto] 音声検出のしきい値（引数なしで -100dB = 常に送信）
  end / start  通話を終える / 始める
  cycle N      通話の終了と開始を N 回繰り返す（通話が回数枠を使うかの検証）
  rejoin N     同じロビーへの参加を N 回繰り返す（回数枠を使う。上限時のエラーの形の検証）
  quit         終了
)";

// ---------- 本体 ----------

struct App {
    std::string tag;                          // 2 つ動かすときの見分け（[A] / [B]）
    std::string tokenFile = "sdk_token.txt";  // アカウントごとのトークン

    template <class... A>
    void TLog(const char* fmt, A... a) {
        std::string f = tag + fmt;
        Log(f.c_str(), a...);
    }

    std::shared_ptr<discordpp::Client> client = std::make_shared<discordpp::Client>();
    uint64_t clientId = 0;
    std::string secret = "dpan-probe";
    bool rpcScopes = false;
    uint64_t lobbyId = 0;
    std::optional<discordpp::Call> call;
    int joinCalls = 0, startCalls = 0;
    int cycleLeft = 0, rejoinLeft = 0;
    bool waitingEnd = false;
    std::map<std::string, std::string> memberMeta;
    Clock::time_point lastStats = Clock::now();
    std::map<uint64_t, Clock::time_point> metaSentAt;

    std::string NameOf(uint64_t userId) {
        if (auto lobby = client->GetLobbyHandle(lobbyId)) {
            if (auto m = lobby->GetLobbyMemberHandle(userId)) {
                if (auto u = m->User()) return u->DisplayName();
            }
        }
        return std::to_string(userId);
    }

    void Authorize() {
        auto verifier = client->CreateAuthorizationCodeVerifier();
        discordpp::AuthorizationArgs args;
        args.SetClientId(clientId);
        std::string scopes = discordpp::Client::GetDefaultCommunicationScopes();
        if (rpcScopes) scopes += " rpc rpc.voice.read rpc.voice.write";
        args.SetScopes(scopes);
        args.SetCodeChallenge(verifier.Challenge());
        TLog("認可を開始（スコープ: %s）。ブラウザか Discord で許可してください", scopes.c_str());
        client->Authorize(args, [this, verifier](discordpp::ClientResult r, std::string code, std::string redirectUri) {
            if (!r.Successful()) {
                TLog("認可に失敗: %s", Describe(r).c_str());
                return;
            }
            client->GetToken(clientId, code, verifier.Verifier(), redirectUri,
                             [this](discordpp::ClientResult r, std::string access, std::string refresh,
                                    discordpp::AuthorizationTokenType, int32_t expiresIn, std::string scopes) {
                                 if (!r.Successful()) {
                                     TLog("トークン交換に失敗（Public Client が有効か確認）: %s", Describe(r).c_str());
                                     return;
                                 }
                                 TLog("トークン取得（client_secret なし／PKCE）。有効期限 %d 秒、スコープ: %s", expiresIn,
                                     scopes.c_str());
                                 SaveToken(tokenFile, {access, refresh, scopes, NowSecs() + expiresIn});
                                 UseToken(access);
                             });
        });
    }

    void UseToken(const std::string& access) {
        client->UpdateToken(discordpp::AuthorizationTokenType::Bearer, access, [this](discordpp::ClientResult r) {
            if (!r.Successful()) {
                TLog("UpdateToken に失敗: %s", Describe(r).c_str());
                return;
            }
            client->Connect();
        });
    }

    void Login(bool relogin) {
        auto saved = relogin ? std::nullopt : LoadToken(tokenFile);
        if (!saved) return Authorize();
        TLog("保存済みトークンを使用（スコープ: %s）", saved->scopes.c_str());
        if (saved->expiresAt - NowSecs() > 24 * 3600) return UseToken(saved->access);
        TLog("期限が近いので refresh_token で更新");
        client->RefreshToken(clientId, saved->refresh,
                             [this](discordpp::ClientResult r, std::string access, std::string refresh,
                                    discordpp::AuthorizationTokenType, int32_t expiresIn, std::string scopes) {
                                 if (!r.Successful()) {
                                     TLog("更新に失敗: %s → 認可からやり直し", Describe(r).c_str());
                                     return Authorize();
                                 }
                                 SaveToken(tokenFile, {access, refresh, scopes, NowSecs() + expiresIn});
                                 UseToken(access);
                             });
    }

    void JoinLobby(std::function<void()> then = {}) {
        joinCalls++;
        auto sentAt = Clock::now();
        client->CreateOrJoinLobbyWithMetadata(
          secret, {}, {memberMeta.begin(), memberMeta.end()},
          [this, then, sentAt](discordpp::ClientResult r, uint64_t id) {
              double ms = std::chrono::duration<double, std::milli>(Clock::now() - sentAt).count();
              if (!r.Successful()) {
                  TLog("ロビー参加 #%d に失敗（%.0fms）: %s", joinCalls, ms, Describe(r).c_str());
                  rejoinLeft = 0;
                  return;
              }
              lobbyId = id;
              TLog("ロビー参加 #%d 成功（%.0fms）lobby=%llu", joinCalls, ms, (unsigned long long)id);
              if (then) then();
          });
    }

    void StartCall() {
        startCalls++;
        call = client->StartCallWithAudioCallbacks(lobbyId, OnAudioReceived, OnAudioCaptured);
        if (!*call) {
            TLog("StartCall #%d: null（すでに通話中）", startCalls);
            return;
        }
        TLog("StartCall #%d", startCalls);
        call->SetStatusChangedCallback([this](discordpp::Call::Status s, discordpp::Call::Error e, int32_t d) {
            TLog("通話の状態: %s（error=%s detail=%d）", discordpp::Call::StatusToString(s).c_str(),
                discordpp::Call::ErrorToString(e).c_str(), d);
            if (s == discordpp::Call::Status::Connected && cycleLeft > 0) EndCall();
        });
        call->SetParticipantChangedCallback([this](uint64_t userId, bool added) {
            TLog("通話の参加者 %s: %s", added ? "追加" : "削除", NameOf(userId).c_str());
        });
        call->SetSpeakingStatusChangedCallback([this](uint64_t userId, bool speaking) {
            TLog("話し中 %s: %s", speaking ? "開始" : "終了", NameOf(userId).c_str());
        });
        call->SetOnVoiceStateChangedCallback([this](uint64_t userId) {
            if (auto vs = call->GetVoiceStateHandle(userId)) {
                TLog("ボイス状態: %s selfMute=%d selfDeaf=%d", NameOf(userId).c_str(), vs->SelfMute(), vs->SelfDeaf());
            }
        });
    }

    void EndCall() {
        waitingEnd = true;
        client->EndCall(lobbyId, [this] {
            waitingEnd = false;
            TLog("EndCall 完了");
            if (cycleLeft > 0) {
                cycleLeft--;
                StartCall();
            }
        });
    }

    std::vector<discordpp::AudioDevice> inputDevices;

    void ShowInputDevices() {
        client->GetCurrentInputDevice([this](discordpp::AudioDevice cur) {
            std::string curId = cur ? cur.Id() : "";
            TLog("SDK が使っている入力デバイス: %s", cur ? cur.Name().c_str() : "（不明）");
            client->GetInputDevices([this, curId](std::vector<discordpp::AudioDevice> devs) {
                inputDevices = devs;
                for (size_t i = 0; i < devs.size(); ++i) {
                    TLog("  [%zu] %s%s%s", i, devs[i].Name().c_str(), devs[i].IsDefault() ? "（Windows の既定）" : "",
                        devs[i].Id() == curId ? " ← 使用中" : "");
                }
            });
        });
    }

    void PrintMembers() {
        auto lobby = client->GetLobbyHandle(lobbyId);
        if (!lobby) return TLog("ロビーがありません");
        std::string lm;
        for (auto& [k, v] : lobby->Metadata()) lm += k + "=" + v + " ";
        TLog("ロビー %llu のメタデータ: %s", (unsigned long long)lobbyId, lm.c_str());
        for (auto id : lobby->LobbyMemberIds()) {
            auto m = lobby->GetLobbyMemberHandle(id);
            std::string meta;
            if (m) {
                for (auto& [k, v] : m->Metadata()) meta += k + "=" + v + " ";
            }
            TLog("  %-20s %llu %s %s", NameOf(id).c_str(), (unsigned long long)id,
                m && m->Connected() ? "接続中" : "未接続", meta.c_str());
        }
    }

    void PrintStats() {
        std::lock_guard lock(g_statsMutex);
        float l, r;
        CurrentPan(l, r);
        TLog("受信: %zu 人 / マイク送出 %llu 回（%llu ch、peak %.2f%s）/ 掛けている pan {%.2f, %.2f}", g_stats.size(),
            (unsigned long long)g_capturedCallbacks.load(), (unsigned long long)g_capturedChannels.load(),
            g_capturedPeak.load(), g_capturedPeak.load() < 0.001f ? " ← 無音" : "", l, r);
        g_capturedPeak = 0;
        if (call && *call) {
            auto vad = call->GetVADThreshold();
            TLog("  自分: selfMute=%d selfDeaf=%d muteAll=%d deafAll=%d / モード=%s / VAD %s %.1fdB / 参加者 %zu 人",
                call->GetSelfMute(), call->GetSelfDeaf(), client->GetSelfMuteAll(), client->GetSelfDeafAll(),
                call->GetAudioMode() == discordpp::AudioModeType::MODE_PTT ? "PTT" : "音声検出",
                vad.Automatic() ? "自動" : "手動", vad.VadThreshold(), call->GetParticipants().size());
        }
        for (auto& [id, s] : g_stats) {
            std::string route = s.channels == 2 ? "その場で pan" : "SDK がそのまま再生";
            if (s.channels == 1 && g_panMode != PanMode::Off) {
                std::lock_guard mixLock(g_mixMutex);
                if (auto it = g_mix.find(id); it != g_mix.end()) {
                    char buf[160];
                    std::snprintf(buf, sizeof buf, "自前で再生（バッファ %.0fms / 途切れ %llu 回 / 破棄 %llu 回%s）",
                                  it->second.samples.size() * 1000.0 / kOutRate,
                                  (unsigned long long)it->second.underruns, (unsigned long long)it->second.drops,
                                  g_outputRunning ? "" : " / 出力未開始");
                    route = buf;
                }
            }
            TLog("  %-20s %llu ch / %d Hz / %llu samples/cb / %llu cb / peak %.2f / %s", NameOf(id).c_str(),
                (unsigned long long)s.channels, s.rate, (unsigned long long)s.samplesPerChannel,
                (unsigned long long)s.callbacks, s.peak, route.c_str());
            s.peak = 0;
        }
    }

    void Command(const std::string& line) {
        std::istringstream in(line);
        std::string cmd;
        in >> cmd;
        if (cmd == "pan") {
            float l, r;
            if (in >> l >> r) {
                g_panL = l;
                g_panR = r;
                g_panMode = PanMode::Fixed;
                TLog("pan {%.2f, %.2f}", l, r);
            }
        } else if (cmd == "sweep") {
            g_panMode = PanMode::Sweep;
            TLog("sweep");
        } else if (cmd == "off") {
            g_panMode = PanMode::Off;
            TLog("pan なし");
        } else if (cmd == "stats") {
            PrintStats();
        } else if (cmd == "members") {
            PrintMembers();
        } else if (cmd == "meta") {
            std::string kv;
            in >> kv;
            auto eq = kv.find('=');
            if (eq == std::string::npos) return TLog("meta K=V の形で指定");
            memberMeta[kv.substr(0, eq)] = kv.substr(eq + 1);
            memberMeta["sent_ms"] = std::to_string(
              std::chrono::duration_cast<std::chrono::milliseconds>(std::chrono::system_clock::now().time_since_epoch())
                .count());
            JoinLobby([this] { TLog("メタデータを書きました（相手側の members / 更新通知で確認）"); });
        } else if (cmd == "devices") {
            ShowInputDevices();
        } else if (cmd == "input") {
            int n = -1;
            in >> n;
            if (n < 0 || n >= static_cast<int>(inputDevices.size())) return TLog("先に devices で番号を確認");
            auto dev = inputDevices[n];
            client->SetInputDevice(dev.Id(), [this, name = dev.Name()](discordpp::ClientResult r) {
                if (r.Successful()) TLog("入力デバイスを切り替え: %s", name.c_str());
                else TLog("入力デバイスを切り替えられません: %s", Describe(r).c_str());
            });
        } else if (cmd == "unmute") {
            if (call && *call) call->SetSelfMute(false);
            client->SetSelfMuteAll(false);
            TLog("ミュートを解除しました");
        } else if (cmd == "vad") {
            std::string arg;
            in >> arg;
            if (!call || !*call) return;
            if (arg == "auto") call->SetVADThreshold(true, 0);
            else call->SetVADThreshold(false, arg.empty() ? -100.0f : std::stof(arg));
            TLog("VAD を %s に設定", arg.empty() ? "-100dB（常に送信）" : arg.c_str());
        } else if (cmd == "end") {
            EndCall();
        } else if (cmd == "start") {
            StartCall();
        } else if (cmd == "cycle") {
            in >> cycleLeft;
            TLog("通話の終了と開始を %d 回繰り返します", cycleLeft);
            if (cycleLeft > 0) EndCall();
        } else if (cmd == "rejoin") {
            in >> rejoinLeft;
            TLog("ロビー参加を %d 回繰り返します（回数枠を使います）", rejoinLeft);
            RejoinNext();
        } else if (cmd == "quit") {
            g_running = false;
        } else if (!cmd.empty()) {
            std::printf("%s", kHelp);
        }
    }

    void RejoinNext() {
        if (rejoinLeft <= 0) return;
        rejoinLeft--;
        JoinLobby([this] { RejoinNext(); });
    }

    void Setup() {
        client->AddLogCallback(
          [this](std::string msg, discordpp::LoggingSeverity) { TLog("[SDK] %s", Trim(msg).c_str()); },
          discordpp::LoggingSeverity::Warning);
        client->SetStatusChangedCallback([this](discordpp::Client::Status s, discordpp::Client::Error e, int32_t d) {
            TLog("クライアントの状態: %s（error=%s detail=%d）", discordpp::Client::StatusToString(s).c_str(),
                discordpp::Client::ErrorToString(e).c_str(), d);
            if (s != discordpp::Client::Status::Ready) return;
            if (auto me = client->GetCurrentUserV2()) {
                TLog("ログイン: %s（%llu）", me->DisplayName().c_str(), (unsigned long long)me->Id());
                memberMeta["name"] = me->DisplayName();
            }
            JoinLobby([this] {
                PrintMembers();
                StartCall();
                ShowInputDevices();
            });
        });
        client->SetLobbyMemberAddedCallback([this](uint64_t, uint64_t m) { TLog("メンバー追加: %s", NameOf(m).c_str()); });
        client->SetLobbyMemberRemovedCallback(
          [this](uint64_t, uint64_t m) { TLog("メンバー削除: %s", NameOf(m).c_str()); });
        client->SetLobbyMemberUpdatedCallback([this](uint64_t lobby, uint64_t m) {
            std::string meta, delay;
            if (auto l = client->GetLobbyHandle(lobby)) {
                if (auto h = l->GetLobbyMemberHandle(m)) {
                    auto md = h->Metadata();
                    for (auto& [k, v] : md) meta += k + "=" + v + " ";
                    if (auto it = md.find("sent_ms"); it != md.end()) {
                        auto now = std::chrono::duration_cast<std::chrono::milliseconds>(
                                     std::chrono::system_clock::now().time_since_epoch())
                                     .count();
                        delay = "（書き込みから約 " + std::to_string(now - std::stoll(it->second)) + "ms）";
                    }
                }
            }
            TLog("メンバー更新: %s %s%s", NameOf(m).c_str(), meta.c_str(), delay.c_str());
        });
        client->SetLobbyUpdatedCallback([this](uint64_t) { TLog("ロビー更新"); });
        client->SetVoiceParticipantChangedCallback([this](uint64_t, uint64_t m, bool added) {
            TLog("ボイス参加者 %s: %s", added ? "参加" : "退出", NameOf(m).c_str());
        });
    }

    void Tick() {
        if (Clock::now() - lastStats > std::chrono::seconds(5) && lobbyId) {
            lastStats = Clock::now();
            PrintStats();
        }
    }
};

int main(int argc, char** argv) {
    SetConsoleOutputCP(CP_UTF8);
    g_logFile = _wfsopen((ExeDir() / "lobby-probe.log").c_str(), L"w", _SH_DENYNO);
    SetConsoleCP(CP_UTF8);
    App app, sub;
    bool relogin = false, reloginSub = false, dual = false;
    sub.secret = "dpan-probe-2";
    for (int i = 1; i < argc; ++i) {
        std::string a = argv[i];
        if (a == "--secret" && i + 1 < argc) app.secret = argv[++i];
        else if (a == "--secret2" && i + 1 < argc) sub.secret = argv[++i];
        else if (a == "--rpc-scopes") app.rpcScopes = true;
        else if (a == "--relogin") relogin = true;
        else if (a == "--relogin2") reloginSub = true;
        else if (a == "--dual") dual = true;
        else if (a == "--b") {  // 2 つ目のアカウント（B）だけでログインする。--dual の前に B の許可を済ませるため
            app.tokenFile = "sdk_token_b.txt";
            app.secret = "dpan-probe-2";
            app.tag = "[B] ";
        }
        else {
            std::printf("lobby-probe [--secret S] [--rpc-scopes] [--relogin]\n"
                        "            [--b]                              2 つ目のアカウント（B）だけでログインする\n"
                        "            [--dual [--secret2 S] [--relogin2]]  2 つ目のアカウントで別のロビーにも入る\n%s",
                        kHelp);
            return 0;
        }
    }
    app.clientId = sub.clientId = LoadClientId();
    if (!app.clientId) {
        std::printf("client_id が見つかりません（.env の DPAN_CLIENT_ID か exe と同じ場所の client_id.txt）\n");
        return 1;
    }
    if (dual) {
        app.tag = "[A] ";
        sub.tag = "[B] ";
        sub.tokenFile = "sdk_token_b.txt";
    }
    Log("client_id=%llu / 合言葉=%s%s%s", (unsigned long long)app.clientId, app.secret.c_str(),
        dual ? " / 2 つ目の合言葉=" : "", dual ? sub.secret.c_str() : "");
    std::printf("%s", kHelp);
    if (dual) std::printf("  （2 つ目のクライアントへのコマンドは先頭に b を付ける。例: b stats）\n");
    std::vector<App*> apps{&app};
    if (dual) apps.push_back(&sub);
    for (auto* a : apps) a->Setup();
    // 2 つ同時に許可を始めるとブラウザが取り合うので、1 つ目のログインが済んでから 2 つ目を始める
    app.Login(relogin);
    bool subStarted = !dual;
    std::thread(InputThread).detach();
    while (g_running) {
        discordpp::RunCallbacks();  // プロセス全体で 1 つ。全クライアントのコールバックがここで動く
        if (!subStarted && app.client->GetStatus() == discordpp::Client::Status::Ready) {
            subStarted = true;
            sub.Login(reloginSub);
        }
        std::deque<std::string> cmds;
        {
            std::lock_guard lock(g_cmdMutex);
            cmds.swap(g_cmds);
        }
        for (auto& c : cmds) {
            if (dual && c.rfind("b ", 0) == 0) sub.Command(c.substr(2));
            else app.Command(c);
        }
        for (auto* a : apps) a->Tick();
        std::this_thread::sleep_for(std::chrono::milliseconds(10));
    }
    for (auto* a : apps) a->TLog("集計: ロビー参加 %d 回 / StartCall %d 回", a->joinCalls, a->startCalls);
    return 0;
}
