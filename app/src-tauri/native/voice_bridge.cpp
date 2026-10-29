#define DISCORDPP_IMPLEMENTATION
#include "discordpp.h"

#include "voice_bridge.h"

#include <algorithm>
#include <cmath>
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstdio>
#include <deque>
#include <filesystem>
#include <fstream>
#include <functional>
#include <map>
#include <mutex>
#include <optional>
#include <shared_mutex>
#include <sstream>
#include <string>
#include <thread>
#include <unordered_map>
#include <vector>

namespace {

using Meta = std::unordered_map<std::string, std::string>;

Meta ParseMeta(const char* s) {
    Meta m;
    if (!s) return m;
    std::string all(s);
    size_t pos = 0;
    while (pos < all.size()) {
        size_t end = all.find('\x1e', pos);
        if (end == std::string::npos) end = all.size();
        std::string item = all.substr(pos, end - pos);
        size_t sep = item.find('\x1f');
        if (sep != std::string::npos) m[item.substr(0, sep)] = item.substr(sep + 1);
        pos = end + 1;
    }
    return m;
}

std::string Esc(const std::string& s) {
    std::string o;
    o.reserve(s.size() + 2);
    for (unsigned char c : s) {
        switch (c) {
        case '"': o += "\\\""; break;
        case '\\': o += "\\\\"; break;
        case '\n': o += "\\n"; break;
        case '\r': o += "\\r"; break;
        case '\t': o += "\\t"; break;
        default:
            if (c < 0x20) {
                char buf[8];
                std::snprintf(buf, sizeof buf, "\\u%04x", c);
                o += buf;
            } else {
                o += static_cast<char>(c);
            }
        }
    }
    return o;
}

std::string Q(const std::string& s) { return "\"" + Esc(s) + "\""; }
std::string Q(uint64_t v) { return "\"" + std::to_string(v) + "\""; }

int64_t NowSecs() {
    return std::chrono::duration_cast<std::chrono::seconds>(std::chrono::system_clock::now().time_since_epoch())
      .count();
}

struct SavedToken {
    std::string access, refresh, scopes;
    int64_t expiresAt = 0;
};

// Discord 本体（RPC）にも同じトークンでログインするため、ロビー通話のスコープに rpc のスコープを足す
const char* kRpcScopes = " rpc rpc.voice.read rpc.voice.write";

class Host;

// 1 つの Discord アカウント（ログイン・ロビー・通話）。Host のスレッドだけから触る
class Account {
public:
    Account(Host* host, int idx, uint64_t appId, std::string tokenPath)
        : host_(host), idx_(idx), appId_(appId), tokenPath_(std::move(tokenPath)) {}

    // ---- コマンド（ブリッジのスレッドで実行） ----

    void Join(const std::string& secret, Meta meta) {
        memberMeta_ = std::move(meta);
        if (secret == secret_ && call_ && *call_) {
            Diag("join " + secret + "：すでにこの通話にいる");
            return;
        }
        EndCurrentCall();
        secret_ = secret;
        if (!ready_) {
            Diag("join " + secret + "：ログイン前なので、ログインが済んだら参加する");
            return;
        }
        // 参加済みでロビーが残っていれば、参加し直さずに通話だけ始める（回数枠を使わない）
        if (auto it = lobbyBySecret_.find(secret); it != lobbyBySecret_.end() && client_->GetLobbyHandle(it->second)) {
            Diag("join " + secret + "：参加済みのロビー " + std::to_string(it->second) + " で通話だけ始める");
            StartCall(it->second);
            return;
        }
        Diag("join " + secret + "：ロビーに参加する（CreateOrJoinLobby、回数枠を 1 使う）");
        CreateOrJoin(secret, true);
    }

    void LeaveCall() {
        Diag("leave：通話を終える（ロビーのメンバーのまま）");
        EndCurrentCall();
        secret_.clear();
    }

    void SetMetadata(Meta meta) {
        memberMeta_ = std::move(meta);
        std::string kv;
        for (auto& [k, v] : memberMeta_) kv += k + "=" + v + " ";
        Diag("メタデータを書く（回数枠を 1 使う）：" + kv);
        if (!secret_.empty()) CreateOrJoin(secret_, false);
    }

    void SetSelfMute(bool m) {
        Diag(std::string("自分のミュート ") + (m ? "on" : "off") + (ready_ ? "" : "（ログイン後に反映）"));
        if (ready_) client_->SetSelfMuteAll(m);
        wantMute_ = m;
    }

    void SetSelfDeaf(bool d) {
        Diag(std::string("自分のスピーカーミュート ") + (d ? "on" : "off") + (ready_ ? "" : "（ログイン後に反映）"));
        if (ready_) client_->SetSelfDeafAll(d);
        wantDeaf_ = d;
    }

    void RequestDevices() {
        client_->GetCurrentInputDevice([this](discordpp::AudioDevice in) {
            std::string curIn = in ? in.Id() : "";
            client_->GetCurrentOutputDevice([this, curIn](discordpp::AudioDevice out) {
                std::string curOut = out ? out.Id() : "";
                client_->GetInputDevices([this, curIn, curOut](std::vector<discordpp::AudioDevice> ins) {
                    client_->GetOutputDevices([this, curIn, curOut, ins](std::vector<discordpp::AudioDevice> outs) {
                        auto list = [](const std::vector<discordpp::AudioDevice>& v) {
                            std::string s = "[";
                            for (size_t i = 0; i < v.size(); ++i) {
                                if (i) s += ",";
                                s += "{\"id\":" + Q(v[i].Id()) + ",\"name\":" + Q(v[i].Name()) +
                                     ",\"default\":" + (v[i].IsDefault() ? "true" : "false") + "}";
                            }
                            return s + "]";
                        };
                        Emit("{\"type\":\"devices\",\"input\":" + list(ins) + ",\"output\":" + list(outs) +
                             ",\"current_input\":" + Q(curIn) + ",\"current_output\":" + Q(curOut) + "}");
                    });
                });
            });
        });
    }

    void SetInputDevice(const std::string& id) {
        Diag("マイクを切り替える：" + id);
        client_->SetInputDevice(id, [this, id](discordpp::ClientResult r) {
            if (!r.Successful()) EmitError("input_device", r);
            else Emit("{\"type\":\"device_set\",\"kind\":\"input\",\"id\":" + Q(id) + "}");
        });
    }

    // パスは UTF-8 で受け取るので、Windows の ANSI コードページで解釈されないよう u8string から作る
    std::filesystem::path TokenPath() const {
        return std::filesystem::path(std::u8string(tokenPath_.begin(), tokenPath_.end()));
    }

    void Reauthorize() {
        Diag("ログインし直す：保存したトークンを捨てて許可から");
        // 許可の手続きが重なると片方が拒否になるので、進行中のものは取り消す
        client_->AbortAuthorize();
        std::error_code ec;
        std::filesystem::remove(TokenPath(), ec);
        EndCurrentCall();
        ready_ = false;
        client_->Disconnect();
        Authorize();
    }

    void SetOutputDevice(const std::string& id) {
        Diag("スピーカーを切り替える：" + id);
        client_->SetOutputDevice(id, [this, id](discordpp::ClientResult r) {
            if (!r.Successful()) EmitError("output_device", r);
            else Emit("{\"type\":\"device_set\",\"kind\":\"output\",\"id\":" + Q(id) + "}");
        });
    }

    // 5 秒ごとの統計（Host のループから）
    void Tick() {
        if (std::chrono::steady_clock::now() - statsSince_ > std::chrono::seconds(5)) EmitAudioStats();
    }

    // 終了・取り外しの前に通話を終える
    void Shutdown() {
        EndCurrentCall();
        if (client_) client_->Disconnect();
    }

    void Start();

private:
    // 受信音声（ボイススレッド）。目標の倍率へ 1 バッファ内で直線的に寄せ、音のぶつ切りを防ぐ
    void OnAudio(uint64_t user, int16_t* data, uint64_t frames, int32_t rate, uint64_t channels, bool&) {
        {
            float peak = 0;
            for (uint64_t i = 0; i < frames * channels; ++i) peak = std::max(peak, std::abs(data[i] / 32768.0f));
            std::lock_guard lock(statsMutex_);
            auto& st = recvStats_[user];
            st.callbacks++;
            st.channels = channels;
            st.rate = rate;
            st.peak = std::max(st.peak, peak);
        }
        if (channels != 2 || frames == 0) return;
        std::pair<float, float> target = GainOf(user);
        auto& last = lastGain_[user];  // ボイススレッドだけが触る
        if (!last.second) last = {target, true};
        auto [l0, r0] = last.first;
        auto [l1, r1] = target;
        for (uint64_t i = 0; i < frames; ++i) {
            float t = static_cast<float>(i + 1) / static_cast<float>(frames);
            float l = l0 + (l1 - l0) * t, r = r0 + (r1 - r0) * t;
            data[2 * i] = static_cast<int16_t>(std::clamp(data[2 * i] * l, -32768.0f, 32767.0f));
            data[2 * i + 1] = static_cast<int16_t>(std::clamp(data[2 * i + 1] * r, -32768.0f, 32767.0f));
        }
        last.first = target;
    }

    void Emit(const std::string& json);
    std::pair<float, float> GainOf(uint64_t user);

    // 切り分け用のログ（Rust 側で info として記録する）
    void Diag(const std::string& msg) { Emit("{\"type\":\"log\",\"message\":" + Q("[bridge] " + msg) + "}"); }

    // 5 秒ごとの音声の状況：話者ごとの受信、掛けている倍率、自分のマイク、デバイス、ミュート
    void EmitAudioStats() {
        std::ostringstream o;
        o.setf(std::ios::fixed);
        o.precision(2);
        double secs = std::chrono::duration<double>(std::chrono::steady_clock::now() - statsSince_).count();
        statsSince_ = std::chrono::steady_clock::now();
        o << "[audio] 通話=" << ((call_ && *call_) ? discordpp::Call::StatusToString(call_->GetStatus()) : std::string("なし"))
          << " 参加者=" << ((call_ && *call_) ? call_->GetParticipants().size() : 0)
          << " muteAll=" << (ready_ && client_->GetSelfMuteAll()) << " deafAll=" << (ready_ && client_->GetSelfDeafAll())
          << " 出力音量=" << (ready_ ? client_->GetOutputVolume() : -1.0f) << " 入力音量=" << (ready_ ? client_->GetInputVolume() : -1.0f);
        {
            std::lock_guard lock(statsMutex_);
            o << " | マイク " << capCallbacks_ / std::max(secs, 0.001) << "回/秒 " << capChannels_ << "ch peak=" << capPeak_;
            capCallbacks_ = 0;
            capPeak_ = 0;
            if (recvStats_.empty()) o << " | 受信なし";
            for (auto& [user, st] : recvStats_) {
                std::pair<float, float> g = GainOf(user);
                o << " | 受信 " << NameOf(user) << "(" << user << ") " << st.callbacks / std::max(secs, 0.001) << "回/秒 "
                  << st.channels << "ch " << st.rate << "Hz peak=" << st.peak << " 倍率={" << g.first << ", " << g.second << "}";
                st.callbacks = 0;
                st.peak = 0;
            }
        }
        Diag(o.str());
    }

    void EmitError(const char* stage, const discordpp::ClientResult& r) {
        std::ostringstream o;
        o << "{\"type\":\"error\",\"stage\":" << Q(stage) << ",\"message\":" << Q(r.Error())
          << ",\"code\":" << r.ErrorCode() << ",\"http\":" << static_cast<int>(r.Status())
          << ",\"retryable\":" << (r.Retryable() ? "true" : "false") << ",\"retry_after\":" << r.RetryAfter() << "}";
        Emit(o.str());
    }

    std::string NameOf(uint64_t user) {
        if (auto lobby = client_->GetLobbyHandle(lobbyId_)) {
            if (auto m = lobby->GetLobbyMemberHandle(user)) {
                if (auto u = m->User()) return u->DisplayName();
            }
        }
        return {};
    }

    void EmitMember(uint64_t lobbyId, uint64_t user) {
        if (lobbyId != lobbyId_) return;
        auto lobby = client_->GetLobbyHandle(lobbyId);
        if (!lobby) return;
        auto m = lobby->GetLobbyMemberHandle(user);
        if (!m) return;
        std::string meta = "{";
        bool first = true;
        for (auto& [k, v] : m->Metadata()) {
            if (!first) meta += ",";
            first = false;
            meta += Q(k) + ":" + Q(v);
        }
        meta += "}";
        std::string name = m->User() ? m->User()->DisplayName() : "";
        Emit("{\"type\":\"member\",\"user_id\":" + Q(user) + ",\"name\":" + Q(name) +
             ",\"connected\":" + (m->Connected() ? "true" : "false") + ",\"metadata\":" + meta + "}");
    }

    void EmitAllMembers() {
        if (auto lobby = client_->GetLobbyHandle(lobbyId_)) {
            for (auto id : lobby->LobbyMemberIds()) EmitMember(lobbyId_, id);
        }
    }

    void CreateOrJoin(const std::string& secret, bool startCall) {
        client_->CreateOrJoinLobbyWithMetadata(
          secret, {}, memberMeta_, [this, secret, startCall](discordpp::ClientResult r, uint64_t lobbyId) {
              if (!r.Successful()) return EmitError("join", r);
              Diag("ロビー " + std::to_string(lobbyId) + " に参加した（" + secret + "）");
              lobbyBySecret_[secret] = lobbyId;
              if (secret != secret_) return;  // 待っている間に別の VC へ移った
              Emit("{\"type\":\"joined\",\"secret\":" + Q(secret) + ",\"lobby_id\":" + Q(lobbyId) + "}");
              if (startCall && !(call_ && *call_)) StartCall(lobbyId);
          });
    }

    void StartCall(uint64_t lobbyId) {
        lobbyId_ = lobbyId;
        call_ = client_->StartCallWithAudioCallbacks(
          lobbyId,
          [this](uint64_t u, int16_t* d, uint64_t f, int32_t rate, uint64_t ch, bool& mute) {
              OnAudio(u, d, f, rate, ch, mute);
          },
          [this](int16_t* d, uint64_t f, int32_t, uint64_t ch) {
              float peak = 0;
              for (uint64_t i = 0; i < f * ch; ++i) peak = std::max(peak, std::abs(d[i] / 32768.0f));
              std::lock_guard lock(statsMutex_);
              capCallbacks_++;
              capChannels_ = ch;
              capPeak_ = std::max(capPeak_, peak);
          });
        Diag("StartCall lobby=" + std::to_string(lobbyId));
        if (!*call_) {
            Emit("{\"type\":\"error\",\"stage\":\"call\",\"message\":\"StartCall returned null\"}");
            return;
        }
        // 通話のイベントにはロビー ID を付ける。VC を移ったとき、前の通話の終了通知が遅れて届くため
        const std::string lid = Q(lobbyId);
        call_->SetStatusChangedCallback([this, lid](discordpp::Call::Status s, discordpp::Call::Error e, int32_t) {
            Emit("{\"type\":\"call\",\"lobby_id\":" + lid + ",\"status\":" + Q(discordpp::Call::StatusToString(s)) +
                 ",\"error\":" + Q(discordpp::Call::ErrorToString(e)) + "}");
        });
        call_->SetParticipantChangedCallback([this, lid](uint64_t user, bool added) {
            Emit("{\"type\":\"participant\",\"lobby_id\":" + lid + ",\"user_id\":" + Q(user) +
                 ",\"added\":" + (added ? "true" : "false") + ",\"name\":" + Q(NameOf(user)) + "}");
        });
        call_->SetSpeakingStatusChangedCallback([this, lid](uint64_t user, bool speaking) {
            Emit("{\"type\":\"speaking\",\"lobby_id\":" + lid + ",\"user_id\":" + Q(user) +
                 ",\"speaking\":" + (speaking ? "true" : "false") + "}");
        });
        Emit("{\"type\":\"call_started\",\"lobby_id\":" + lid + "}");
        for (auto id : call_->GetParticipants()) {
            Emit("{\"type\":\"participant\",\"lobby_id\":" + lid + ",\"user_id\":" + Q(id) +
                 ",\"added\":true,\"name\":" + Q(NameOf(id)) + "}");
        }
        EmitAllMembers();
    }

    void EndCurrentCall() {
        if (!(call_ && *call_)) return;
        uint64_t lobby = lobbyId_;
        Diag("EndCall lobby=" + std::to_string(lobby));
        call_.reset();
        client_->EndCall(lobby, [this, lobby] { Emit("{\"type\":\"call_ended\",\"lobby_id\":" + Q(lobby) + "}"); });
    }

    // ---- 認可 ----

    std::optional<SavedToken> LoadToken() {
        std::ifstream f(TokenPath());
        SavedToken t;
        std::string exp;
        if (!std::getline(f, t.access) || !std::getline(f, t.refresh) || !std::getline(f, exp)) return {};
        std::getline(f, t.scopes);
        try {
            t.expiresAt = std::stoll(exp);
        } catch (...) {
            return {};
        }
        return t;
    }

    void SaveToken(const std::string& a, const std::string& r, int32_t expiresIn, const std::string& scopes) {
        std::ofstream f(TokenPath(), std::ios::trunc);
        f << a << "\n" << r << "\n" << NowSecs() + expiresIn << "\n" << scopes << "\n";
    }

    void OnToken(discordpp::ClientResult r, std::string access, std::string refresh, int32_t expiresIn,
                 std::string scopes, bool fallbackToAuthorize) {
        Diag(std::string("トークン取得 ") + (r.Successful() ? "成功" : "失敗：" + r.Error()) + "（有効 " +
             std::to_string(expiresIn) + " 秒、スコープ " + scopes + "）");
        if (!r.Successful()) {
            if (fallbackToAuthorize) return Authorize();
            return EmitError("auth", r);
        }
        SaveToken(access, refresh, expiresIn, scopes);
        UseToken(access, scopes);
    }

    void Authorize() {
        auto verifier = client_->CreateAuthorizationCodeVerifier();
        discordpp::AuthorizationArgs args;
        args.SetClientId(appId_);
        args.SetScopes(discordpp::Client::GetDefaultCommunicationScopes() + kRpcScopes);
        args.SetCodeChallenge(verifier.Challenge());
        Diag("許可を始める（スコープ " + discordpp::Client::GetDefaultCommunicationScopes() + kRpcScopes + "）");
        Emit("{\"type\":\"authorizing\"}");
        client_->Authorize(args, [this, verifier](discordpp::ClientResult r, std::string code, std::string redirect) {
            if (!r.Successful()) return EmitError("auth", r);
            client_->GetToken(appId_, code, verifier.Verifier(), redirect,
                              [this](discordpp::ClientResult r, std::string a, std::string rf,
                                     discordpp::AuthorizationTokenType, int32_t exp, std::string sc) {
                                  OnToken(r, a, rf, exp, sc, false);
                              });
        });
    }

    void UseToken(const std::string& access, const std::string& scopes) {
        // Discord 本体（RPC）のログインにも使うので Rust 側へ渡す
        Emit("{\"type\":\"token\",\"access_token\":" + Q(access) + ",\"scopes\":" + Q(scopes) + "}");
        client_->UpdateToken(discordpp::AuthorizationTokenType::Bearer, access, [this](discordpp::ClientResult r) {
            if (!r.Successful()) return Authorize();
            client_->Connect();
        });
    }

    void Login() {
        auto saved = LoadToken();
        if (!saved) Diag("保存したトークンなし → 許可から");
        else
            Diag("保存したトークンあり（期限まで " + std::to_string((saved->expiresAt - NowSecs()) / 3600) +
                 " 時間、スコープ " + saved->scopes + "）");
        // rpc のスコープがない古いトークンは使わない（Discord 本体にログインできないため）
        if (!saved || saved->scopes.find("rpc.voice.write") == std::string::npos) return Authorize();
        if (saved->expiresAt - NowSecs() > 24 * 3600) return UseToken(saved->access, saved->scopes);
        Diag("期限が近いので更新する");
        client_->RefreshToken(appId_, saved->refresh,
                              [this](discordpp::ClientResult r, std::string a, std::string rf,
                                     discordpp::AuthorizationTokenType, int32_t exp,
                                     std::string sc) { OnToken(r, a, rf, exp, sc, true); });
    }

    Host* host_;
    int idx_;
    uint64_t appId_;
    std::string tokenPath_;

    std::shared_ptr<discordpp::Client> client_;
    bool ready_ = false;
    bool wantMute_ = false, wantDeaf_ = false;
    std::string secret_;
    Meta memberMeta_;
    std::map<std::string, uint64_t> lobbyBySecret_;
    uint64_t lobbyId_ = 0;
    std::optional<discordpp::Call> call_;

    std::unordered_map<uint64_t, std::pair<std::pair<float, float>, bool>> lastGain_;

    struct RecvStats {
        uint64_t callbacks = 0, channels = 0;
        int32_t rate = 0;
        float peak = 0;
    };
    std::mutex statsMutex_;
    std::map<uint64_t, RecvStats> recvStats_;
    uint64_t capCallbacks_ = 0, capChannels_ = 0;
    float capPeak_ = 0;
    std::chrono::steady_clock::time_point statsSince_ = std::chrono::steady_clock::now();
};


// SDK のスレッド。RunCallbacks はプロセス全体で 1 つなので、全アカウントをこの 1 本のスレッドで動かす
class Host {
public:
    Host(uint64_t appId, dpan_event_cb cb, void* ctx) : appId_(appId), cb_(cb), ctx_(ctx) {
        thread_ = std::thread([this] { Run(); });
    }

    ~Host() {
        Post([](Host& h) { h.running_ = false; });
        if (thread_.joinable()) thread_.join();
    }

    void Post(std::function<void(Host&)> task) {
        std::lock_guard lock(tasksMutex_);
        tasks_.push_back(std::move(task));
    }

    int AddAccount(std::string tokenPath) {
        int idx = nextIdx_++;
        Post([idx, tokenPath](Host& h) {
            auto acc = std::make_unique<Account>(&h, idx, h.appId_, tokenPath);
            auto* raw = acc.get();
            h.accounts_[idx] = std::move(acc);
            raw->Start();
        });
        return idx;
    }

    void RemoveAccount(int idx) {
        Post([idx](Host& h) {
            auto it = h.accounts_.find(idx);
            if (it == h.accounts_.end()) return;
            it->second->Shutdown();
            // 終了の通知を送り切ってから破棄する
            h.retiring_.push_back({std::move(it->second), 100});
            h.accounts_.erase(it);
        });
    }

    void With(int idx, std::function<void(Account&)> f) {
        Post([idx, f](Host& h) {
            if (auto it = h.accounts_.find(idx); it != h.accounts_.end()) f(*it->second);
        });
    }

    void Emit(int idx, const std::string& json) {
        // どのアカウントのイベントかを先頭に足す
        std::string out = "{\"account\":" + std::to_string(idx) + (json.size() > 2 ? "," : "") + json.substr(1);
        if (cb_) cb_(out.c_str(), ctx_);
    }

    void SetGain(int idx, uint64_t user, float l, float r) {
        std::unique_lock lock(gainMutex_);
        gains_[{idx, user}] = {l, r};
    }

    void ClearGain(int idx, uint64_t user) {
        std::unique_lock lock(gainMutex_);
        gains_.erase({idx, user});
    }

    std::pair<float, float> Gain(int idx, uint64_t user) {
        std::shared_lock lock(gainMutex_);
        auto it = gains_.find({idx, user});
        return it != gains_.end() ? it->second : std::pair<float, float>{1.0f, 1.0f};
    }

private:
    void Run() {
        while (running_) {
            discordpp::RunCallbacks();
            std::deque<std::function<void(Host&)>> tasks;
            {
                std::lock_guard lock(tasksMutex_);
                tasks.swap(tasks_);
            }
            for (auto& t : tasks) t(*this);
            for (auto& [idx, acc] : accounts_) acc->Tick();
            for (auto it = retiring_.begin(); it != retiring_.end();) {
                if (--it->second <= 0) it = retiring_.erase(it);
                else ++it;
            }
            std::this_thread::sleep_for(std::chrono::milliseconds(10));
        }
        for (auto& [idx, acc] : accounts_) acc->Shutdown();
        for (int i = 0; i < 30; ++i) {  // EndCall を送り切る
            discordpp::RunCallbacks();
            std::this_thread::sleep_for(std::chrono::milliseconds(10));
        }
        retiring_.clear();
        accounts_.clear();
    }

    uint64_t appId_;
    dpan_event_cb cb_;
    void* ctx_;
    std::thread thread_;
    std::mutex tasksMutex_;
    std::deque<std::function<void(Host&)>> tasks_;
    bool running_ = true;
    std::atomic<int> nextIdx_{0};
    std::map<int, std::unique_ptr<Account>> accounts_;
    std::vector<std::pair<std::unique_ptr<Account>, int>> retiring_;
    std::shared_mutex gainMutex_;
    std::map<std::pair<int, uint64_t>, std::pair<float, float>> gains_;
};

void Account::Emit(const std::string& json) { host_->Emit(idx_, json); }

std::pair<float, float> Account::GainOf(uint64_t user) { return host_->Gain(idx_, user); }

void Account::Start() {
        client_ = std::make_shared<discordpp::Client>();
        client_->AddLogCallback(
          [this](std::string msg, discordpp::LoggingSeverity) {
              while (!msg.empty() && (msg.back() == '\n' || msg.back() == '\r')) msg.pop_back();
              Emit("{\"type\":\"log\",\"message\":" + Q(msg) + "}");
          },
          discordpp::LoggingSeverity::Warning);
        client_->SetStatusChangedCallback([this](discordpp::Client::Status s, discordpp::Client::Error e, int32_t d) {
            Emit("{\"type\":\"status\",\"status\":" + Q(discordpp::Client::StatusToString(s)) +
                 ",\"error\":" + Q(discordpp::Client::ErrorToString(e)) + ",\"detail\":" + std::to_string(d) + "}");
            if (s != discordpp::Client::Status::Ready) {
                ready_ = false;
                return;
            }
            ready_ = true;
            client_->SetSelfMuteAll(wantMute_);
            client_->SetSelfDeafAll(wantDeaf_);
            auto me = client_->GetCurrentUserV2();
            Emit("{\"type\":\"ready\",\"user_id\":" + Q(me ? me->Id() : 0) + ",\"name\":" +
                 Q(me ? me->DisplayName() : "") + "}");
            if (!secret_.empty() && !(call_ && *call_)) {
                auto s2 = secret_;
                secret_.clear();
                Join(s2, memberMeta_);
            }
        });
        client_->SetDeviceChangeCallback(
          [this](std::vector<discordpp::AudioDevice>, std::vector<discordpp::AudioDevice>) {
              Diag("オーディオデバイスの構成が変わった → 一覧を取り直す");
              RequestDevices();
          });
        client_->SetLobbyMemberAddedCallback([this](uint64_t l, uint64_t m) { EmitMember(l, m); });
        client_->SetLobbyMemberUpdatedCallback([this](uint64_t l, uint64_t m) { EmitMember(l, m); });
        client_->SetLobbyMemberRemovedCallback([this](uint64_t l, uint64_t m) {
            if (l == lobbyId_) Emit("{\"type\":\"member_removed\",\"user_id\":" + Q(m) + "}");
        });
        Login();
}

Host* H(void* h) { return static_cast<Host*>(h); }
std::string S(const char* s) { return s ? s : ""; }

}  // namespace

extern "C" {

void* dpan_create(uint64_t application_id, dpan_event_cb cb, void* ctx) { return new Host(application_id, cb, ctx); }

void dpan_destroy(void* h) { delete H(h); }

int dpan_add_account(void* h, const char* token_path) { return H(h)->AddAccount(S(token_path)); }

void dpan_remove_account(void* h, int account) { H(h)->RemoveAccount(account); }

void dpan_join(void* h, int account, const char* secret, const char* meta) {
    H(h)->With(account, [s = S(secret), m = ParseMeta(meta)](Account& a) { a.Join(s, m); });
}

void dpan_leave_call(void* h, int account) {
    H(h)->With(account, [](Account& a) { a.LeaveCall(); });
}

void dpan_set_metadata(void* h, int account, const char* meta) {
    H(h)->With(account, [m = ParseMeta(meta)](Account& a) { a.SetMetadata(m); });
}

void dpan_set_gain(void* h, int account, uint64_t user_id, float left, float right) {
    H(h)->SetGain(account, user_id, left, right);
}

void dpan_clear_gain(void* h, int account, uint64_t user_id) { H(h)->ClearGain(account, user_id); }

void dpan_set_self_mute(void* h, int account, int mute) {
    H(h)->With(account, [mute](Account& a) { a.SetSelfMute(mute != 0); });
}

void dpan_set_self_deaf(void* h, int account, int deaf) {
    H(h)->With(account, [deaf](Account& a) { a.SetSelfDeaf(deaf != 0); });
}

void dpan_request_devices(void* h, int account) {
    H(h)->With(account, [](Account& a) { a.RequestDevices(); });
}

void dpan_reauthorize(void* h, int account) {
    H(h)->With(account, [](Account& a) { a.Reauthorize(); });
}

void dpan_set_input_device(void* h, int account, const char* id) {
    H(h)->With(account, [i = S(id)](Account& a) { a.SetInputDevice(i); });
}

void dpan_set_output_device(void* h, int account, const char* id) {
    H(h)->With(account, [i = S(id)](Account& a) { a.SetOutputDevice(i); });
}

}  // extern "C"
