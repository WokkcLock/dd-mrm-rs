// DdHotapply —— DdModTool 热应用原生引擎（v2.1 语义的 C 复刻，2026-11-04）。
// 替代 frida 链：工具注入本 DLL → DLL 建命名管道服务器 + 帧钩（唯一钩点）→
// 工具经管道发 apply → 帧间隙在游戏主线程执行官方链（清向量+sub_140562920 推入+sub_14040F5B0 落盘）。
// 协议/常量/护栏与 Gui/eng/agent.js（frida 版，保留可回退）逐字对齐。
// 安全设计：①先取管道句柄（被占=别的引擎在服务，干净退出不装钩）②FRAME 入口 16 字节模式校验
// ③装钩期间冻结其余线程 ④apply 全程现读指针+bad-vector 护栏 ⑤无帧 2.5s 超时无副作用退出。
// 游戏版本：Darkest.exe MD5 d1bd550c…（模式不符即拒载，不会写错版本）。

#include <windows.h>
#include <stdio.h>
#include <stdarg.h>
#include <stdlib.h>
#include <string.h>
#include <tlhelp32.h>

// ---------------- 常量（与 agent.js 一致） ----------------
#define PIPE_NAME   L"\\\\.\\pipe\\DdModToolHotapply"
#define CFG_MAPNAME L"Local\\DdModToolHotapplyCfg"

#define RVA_FRAME     0x3D93C0
#define RVA_VPUSH     0x562920          // void  fPush(pg容器, 192B条目)
#define RVA_PERSIST   0x40F5B0          // i64   fPersist(屏对象, uint 槽位)
#define RVA_ESTPTR    0x117D8B0         // qword 地产屏全局指针
#define OFF_ENVARRAY  5992              // 屏+5992 = 信封数组指针
#define ENV_SIZE      3088
#define OFF_PG_BASE   328               // 信封+328 = PG base 容器
#define OFF_VEC_BEGIN 520
#define OFF_VEC_END   528
#define ENTRY_SIZE    192               // {title[128], version[64]}
#define OFF_ENV_HASCAMP 325
#define MAX_ENVS      9
#define MAX_ENTRIES   128
#define FRAME_PATCH_LEN 13              // push 序列长度（trampoline 原样复制）

// FRAME 入口 16 字节版本模式（2026-11-04 从本版 exe 提取；不符拒载）
static const unsigned char FRAME_PATTERN[16] = {
    0x40,0x55,0x53,0x56,0x57,0x41,0x54,0x41,0x55,0x41,0x56,0x41,0x57,0x48,0x8d,0xac };

typedef void      (*fn_push)(void*, void*);
typedef long long (*fn_persist)(void*, unsigned int);

static unsigned char* g_base = NULL;    // Darkest.exe 基址
static fn_push    g_fPush    = NULL;
static fn_persist g_fPersist = NULL;

// ---------------- 日志 ----------------
static CRITICAL_SECTION g_logCS;
static wchar_t g_logDir[MAX_PATH] = L"";
static HANDLE g_logFile = INVALID_HANDLE_VALUE;

static void LogOpen(void) {
    HANDLE hMap = OpenFileMappingW(FILE_MAP_READ, FALSE, CFG_MAPNAME);
    if (hMap) {
        wchar_t* p = (wchar_t*)MapViewOfFile(hMap, FILE_MAP_READ, 0, 0, 0);
        if (p) { wcsncpy_s(g_logDir, MAX_PATH, p, _TRUNCATE); UnmapViewOfFile(p); }
        CloseHandle(hMap);
    }
    if (!g_logDir[0]) {
        // 回退：日志写在 DLL 自己旁边（注入器没给目录时的可见性兜底）
        HMODULE hSelf;
        if (GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS |
                               GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                               (LPCWSTR)(void*)&LogOpen, &hSelf)) {
            wchar_t self[MAX_PATH];
            if (GetModuleFileNameW(hSelf, self, MAX_PATH)) {
                wchar_t* slash = wcsrchr(self, L'\\');
                if (slash) *slash = 0;
                wcsncpy_s(g_logDir, MAX_PATH, self, _TRUNCATE);
            }
        }
    }
    if (g_logDir[0]) {
        CreateDirectoryW(g_logDir, NULL);   // 目录不存在则建一层（父目录须存在）
        wchar_t path[MAX_PATH]; swprintf_s(path, MAX_PATH, L"%s\\engine.log", g_logDir);
        g_logFile = CreateFileW(path, FILE_APPEND_DATA, FILE_SHARE_READ | FILE_SHARE_WRITE,
                                NULL, OPEN_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL);
    }
}

static void Log(const char* fmt, ...) {
    if (g_logFile == INVALID_HANDLE_VALUE) return;
    SYSTEMTIME st; GetSystemTime(&st);
    char line[1024];
    int n = snprintf(line, sizeof line, "%04u-%02u-%02uT%02u:%02u:%02u.%03uZ ",
                     st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond, st.wMilliseconds);
    va_list ap; va_start(ap, fmt);
    int m = vsnprintf(line + n, sizeof line - n - 2, fmt, ap);
    va_end(ap);
    if (m < 0) return;
    n += m;
    if (n > (int)sizeof line - 2) n = (int)sizeof line - 2;
    line[n] = '\n'; line[n + 1] = 0;
    EnterCriticalSection(&g_logCS);
    DWORD w; WriteFile(g_logFile, line, (DWORD)strlen(line), &w, NULL);
    LeaveCriticalSection(&g_logCS);
}

// 可打印化（回包/日志用，杜绝 frida 版乱码瑕疵）
static void Sane(char* dst, size_t cap, const char* src, size_t max) {
    size_t o = 0;
    for (size_t i = 0; i < max && src[i] && o + 1 < cap; i++)
        dst[o++] = (src[i] >= 0x20 && src[i] <= 0x7e) ? src[i] : '?';
    dst[o] = 0;
}

// ---------------- 迷你 JSON（固定 schema：行协议） ----------------

static void json_decode_str(const char* s, size_t len, char* dst, size_t cap) {
    size_t o = 0;
    for (size_t i = 0; i < len && s[i] && o + 4 < cap; i++) {
        char c = s[i];
        if (c != '\\') { dst[o++] = c; continue; }
        if (i + 1 >= len) break;
        char e = s[++i];
        switch (e) {
            case '\"': case '\\': case '/': dst[o++] = e; break;
            case 'b': dst[o++] = '\b'; break;  case 'f': dst[o++] = '\f'; break;
            case 'n': dst[o++] = '\n'; break;  case 'r': dst[o++] = '\r'; break;
            case 't': dst[o++] = '\t'; break;
            case 'u': {
                unsigned cp = 0xFFFD;
                if (i + 4 < len) {
                    cp = 0;
                    for (int k = 1; k <= 4; k++) {
                        char h = s[i + k]; unsigned v;
                        if (h >= '0' && h <= '9') v = (unsigned)(h - '0');
                        else if (h >= 'a' && h <= 'f') v = (unsigned)(h - 'a' + 10);
                        else if (h >= 'A' && h <= 'F') v = (unsigned)(h - 'A' + 10);
                        else { cp = 0xFFFD; break; }
                        cp = cp * 16 + v;
                    }
                    i += 4;
                }
                if (cp >= 0xD800 && cp <= 0xDBFF && i + 6 < len &&
                    s[i + 1] == '\\' && s[i + 2] == 'u') {
                    unsigned lo = 0xFFFF; const char* q = s + i + 3;
                    int bad = 0;
                    for (int k = 0; k < 4; k++) {
                        char h = q[k]; unsigned v;
                        if (h >= '0' && h <= '9') v = (unsigned)(h - '0');
                        else if (h >= 'a' && h <= 'f') v = (unsigned)(h - 'a' + 10);
                        else if (h >= 'A' && h <= 'F') v = (unsigned)(h - 'A' + 10);
                        else { bad = 1; break; }
                        lo = lo * 16 + v;
                    }
                    if (!bad && lo >= 0xDC00 && lo <= 0xDFFF) {
                        cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                        i += 6;
                    } else cp = 0xFFFD;
                } else if (cp >= 0xD800 && cp <= 0xDFFF) cp = 0xFFFD;
                if (cp < 0x80) dst[o++] = (char)cp;
                else if (cp < 0x800) { dst[o++] = (char)(0xc0 | cp >> 6); dst[o++] = (char)(0x80 | (cp & 63)); }
                else if (cp < 0x10000) { dst[o++] = (char)(0xe0 | cp >> 12); dst[o++] = (char)(0x80 | ((cp >> 6) & 63)); dst[o++] = (char)(0x80 | (cp & 63)); }
                else { dst[o++] = (char)(0xf0 | cp >> 18); dst[o++] = (char)(0x80 | ((cp >> 12) & 63)); dst[o++] = (char)(0x80 | ((cp >> 6) & 63)); dst[o++] = (char)(0x80 | (cp & 63)); }
                break;
            }
            default: dst[o++] = e; break;
        }
    }
    dst[o] = 0;
}

// 在整行 JSON 里找 "key": 后的字符串值（首个匹配；返回 0=未找到）
static int json_get_str(const char* js, const char* key, char* dst, size_t cap) {
    char pat[32]; snprintf(pat, sizeof pat, "\"%s\"", key);
    const char* p = strstr(js, pat);
    if (!p) return 0;
    p += strlen(pat);
    while (*p == ' ' || *p == '\t') p++;
    if (*p != ':') return 0;
    p++;
    while (*p == ' ' || *p == '\t') p++;
    if (*p != '"') return 0;
    p++;
    const char* q = p; int esc = 0;
    while (*q) {
        if (esc) esc = 0;
        else if (*q == '\\') esc = 1;
        else if (*q == '"') break;
        q++;
    }
    if (*q != '"') return 0;
    json_decode_str(p, (size_t)(q - p), dst, cap);
    return 1;
}

static int json_get_num(const char* js, const char* key, long* out) {
    char pat[32]; snprintf(pat, sizeof pat, "\"%s\"", key);
    const char* p = strstr(js, pat);
    if (!p) return 0;
    p += strlen(pat);
    while (*p == ' ' || *p == '\t') p++;
    if (*p != ':') return 0;
    p++;
    while (*p == ' ' || *p == '\t') p++;
    *out = strtol(p, NULL, 10);
    return 1;
}

// entries 数组抽取：遍历对象里的 title/version 键
typedef struct { char title[160]; char version[72]; } Entry;

static int json_get_entries(const char* js, Entry* ents, int maxN) {
    const char* arr = strstr(js, "\"entries\"");
    if (!arr) return -1;
    arr = strchr(arr, '[');
    if (!arr) return -1;
    int n = 0;
    const char* obj = strchr(arr, '{');
    while (obj && n < maxN) {
        const char* end = strchr(obj, '}');
        if (!end) return -1;
        size_t len = (size_t)(end - obj);
        if (len > 8192) return -1;
        char buf[8448]; memcpy(buf, obj + 1, len - 1); buf[len - 1] = 0;
        Entry* e = &ents[n];
        if (!json_get_str(buf, "title", e->title, sizeof e->title)) return -2;
        if (!json_get_str(buf, "version", e->version, sizeof e->version)) return -2;
        n++;
        obj = strchr(end, '{');
    }
    return n;   // 空数组 [] → 0（=清空全部 mod，已实测游戏容忍）
}

// ---------------- apply 操作（帧线程执行；语义=agent.js executeOfficialApply） ----------------

typedef struct {
    volatile LONG st;              // 0 pending / 2 done（1 隐含于被取走）
    char profile[32];
    int   n;
    Entry ents[MAX_ENTRIES];
    // 结果
    int   ok; int count; int persisted;
    char  state[320];
} Op;

static Op  g_ops[4];
static int g_opIdx = 0;
static Op* volatile g_pending = NULL;
static HANDLE g_doneEv = NULL;
static volatile LONG g_frameHits = 0;

static void utf8_trunc_copy(char* dst, size_t cap, const char* src) {
    size_t n = strlen(src);
    if (n >= cap) n = cap - 1;                 // cap 含 NUL 位（128/64 → 可写 127/63 字节）
    while (n > 0 && ((unsigned char)src[n] & 0xc0) == 0x80) n--;  // 不撕裂 UTF-8 序列
    memcpy(dst, src, n); dst[n] = 0;
}

static void DoApply(Op* op) {
    const char* profile = op->profile;
    op->ok = 0; op->count = 0; op->persisted = 0;

    // ① 现读屏对象（不缓存；case22 会重建）
    void* est = *(void**)(g_base + RVA_ESTPTR);
    if (!est) { strcpy_s(op->state, _countof(op->state), "not-at-list"); return; }
    unsigned char* envBase = *(unsigned char**)((unsigned char*)est + OFF_ENVARRAY);
    if (!envBase) { strcpy_s(op->state, _countof(op->state), "no-envelope-array"); return; }

    // ② 按名字找信封：前缀匹配 profile_N / steam_profile_N（名是定长 char[32] 无 NUL，不能全等）
    int slot = -1; unsigned char* env = NULL;
    char seen[MAX_ENVS][40];
    char steam[40]; snprintf(steam, sizeof steam, "steam_%s", profile);
    size_t plen = strlen(profile), slen = strlen(steam);
    for (int i = 0; i < MAX_ENVS; i++) {
        unsigned char* e = envBase + ENV_SIZE * (size_t)i;
        Sane(seen[i], sizeof seen[i], (char*)e, 32);
        if ((plen <= 32 && strncmp((char*)e, profile, plen) == 0) ||
            (slen <= 32 && strncmp((char*)e, steam, slen) == 0)) {
            slot = i; env = e; break;          // startsWith 语义；槽位 0-8 单数字无前缀歧义
        }
    }
    if (!env) {
        char s[300] = "envelope-not-found: ";
        strncat_s(s, _countof(s), profile, 16); strncat_s(s, _countof(s), " seen=[", 8);
        for (int i = 0; i < MAX_ENVS; i++) {
            if (i) strncat_s(s, _countof(s), ",", 2);
            strncat_s(s, _countof(s), seen[i], 24);
        }
        strncat_s(s, _countof(s), "]", 2);
        strncpy_s(op->state, _countof(op->state), s, _TRUNCATE);
        return;
    }

    unsigned char* pg = env + OFF_PG_BASE;
    unsigned char* begin = *(unsigned char**)(pg + OFF_VEC_BEGIN);
    unsigned char* end   = *(unsigned char**)(pg + OFF_VEC_END);
    long long span = (long long)(end - begin);
    // begin=end=NULL 是合法空向量：清空档的 persist.game.json 无 applied_ugcs 字段，
    // 列表重建重读后容器从未推过条目（游戏自己造出的状态）
    int empty = !begin && !end;
    if (!empty && (!begin || span < 0 || span % ENTRY_SIZE != 0 || span > (long long)ENTRY_SIZE * 1000)) {
        char s[96]; snprintf(s, sizeof s, "bad-vector: %llx..%llx",
                             (unsigned long long)(uintptr_t)begin, (unsigned long long)(uintptr_t)end);
        strcpy_s(op->state, _countof(op->state), s);
        return;
    }
    int hadCampaign = env[OFF_ENV_HASCAMP] != 0;

    // ③ 官方清空：end = begin（不释放容量；192B POD 条目无析构）
    *(void**)(pg + OFF_VEC_END) = begin;

    // ④ 官方推入：逐条 sub_140562920(容器, 条目)；增长/查重/分配在游戏自己代码里
    unsigned char scratch[ENTRY_SIZE];
    for (int i = 0; i < op->n; i++) {
        memset(scratch, 0, sizeof scratch);
        utf8_trunc_copy((char*)scratch,        128, op->ents[i].title);    // title[128]（含 NUL 位）
        utf8_trunc_copy((char*)scratch + 128,   64, op->ents[i].version);  // version[64]
        g_fPush(pg, scratch);
    }
    // 空向量首推时游戏会更新 begin（NULL→新分配），重读再算
    begin = *(unsigned char**)(pg + OFF_VEC_BEGIN);
    unsigned char* newEnd = *(unsigned char**)(pg + OFF_VEC_END);
    op->count = (int)((long long)(newEnd - begin) / ENTRY_SIZE);

    // ⑤ 官方落盘（内含 !demo && env+325 守卫；无战役时跳过，内存补丁仍成立）
    if (hadCampaign) { g_fPersist(est, (unsigned int)slot); op->persisted = 1; }

    op->ok = 1;
    strcpy_s(op->state, _countof(op->state), op->persisted ? "applied" : "applied-no-persist");

    char first[80] = "null";
    if (op->count > 0) { char raw[80]; memcpy(raw, begin, 64); raw[64] = 0; Sane(first, sizeof first, raw, 64); }
    Log("APPLY-DONE %s slot=%d count=%d persisted=%d first=\"%s\"", profile, slot, op->count, op->persisted, first);
}

// thunk64.asm 保存全部易失寄存器后调用本函数，再跳 frame_trampoline（原序言+回原+13）
void frame_op(void);
unsigned char frame_trampoline[64];   // 13B 原序言 + mov rax,imm64;jmp rax（init 填充，RWX）

void frame_op(void) {
    LONG n = InterlockedIncrement(&g_frameHits);
    if (n <= 3) Log("frame alive #%d", n);
    Op* op = (Op*)InterlockedExchangePointer((PVOID volatile*)&g_pending, NULL);
    if (!op) return;
    __try {
        DoApply(op);
    }
    __except (EXCEPTION_EXECUTE_HANDLER) {
        op->ok = 0;
        strcpy_s(op->state, _countof(op->state), "apply-exception");
    }
    InterlockedExchange(&op->st, 2);
    SetEvent(g_doneEv);
}

// ---------------- 帧钩安装 ----------------

extern void frame_thunk(void);   // thunk64.asm

static int FreezeOtherThreads(HANDLE* hArr, DWORD cap) {
    HANDLE snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
    if (snap == INVALID_HANDLE_VALUE) return 0;
    DWORD me = GetCurrentProcessId(), meT = GetCurrentThreadId();
    THREADENTRY32 te; te.dwSize = sizeof te;
    int n = 0;
    if (Thread32First(snap, &te)) {
        do {
            if (te.th32OwnerProcessID == me && te.th32ThreadID != meT && n < (int)cap) {
                HANDLE h = OpenThread(THREAD_SUSPEND_RESUME, FALSE, te.th32ThreadID);
                if (h) { if (SuspendThread(h) != (DWORD)-1) hArr[n++] = h; else CloseHandle(h); }
            }
        } while (Thread32Next(snap, &te));
    }
    CloseHandle(snap);
    return n;
}
static void ResumeThreads(HANDLE* hArr, int n) {
    for (int i = n - 1; i >= 0; i--) { ResumeThread(hArr[i]); CloseHandle(hArr[i]); }
}

static int InstallFrameHook(void) {
    unsigned char* target = g_base + RVA_FRAME;
    DWORD oldp;
    // trampoline：原 13 字节 push 序列 + mov rax,imm64;jmp rax（回原+13）
    memcpy(frame_trampoline, target, FRAME_PATCH_LEN);
    unsigned char* p = frame_trampoline + FRAME_PATCH_LEN;
    *p++ = 0x48; *p++ = 0xB8;
    *(void**)p = target + FRAME_PATCH_LEN; p += 8;
    *p++ = 0xFF; *p++ = 0xE0;
    VirtualProtect(frame_trampoline, sizeof frame_trampoline, PAGE_EXECUTE_READWRITE, &oldp);

    // patch：mov rax,imm64(frame_thunk); jmp rax; nop
    unsigned char patch[FRAME_PATCH_LEN];
    unsigned char* q = patch;
    *q++ = 0x48; *q++ = 0xB8;
    *(void**)q = (void*)&frame_thunk; q += 8;
    *q++ = 0xFF; *q++ = 0xE0;
    *q++ = 0x90;

    HANDLE frozen[256];
    int nF = FreezeOtherThreads(frozen, 256);
    VirtualProtect(target, FRAME_PATCH_LEN, PAGE_EXECUTE_READWRITE, &oldp);
    memcpy(target, patch, FRAME_PATCH_LEN);
    FlushInstructionCache(GetCurrentProcess(), target, FRAME_PATCH_LEN);
    FlushInstructionCache(GetCurrentProcess(), frame_trampoline, sizeof frame_trampoline);
    ResumeThreads(frozen, nF);
    return 1;
}

// ---------------- 管道服务器 ----------------

static void Reply(HANDLE h, long id, int ok, const char* state, int count) {
    char buf[512];
    int n;
    if (count >= 0)
        n = snprintf(buf, sizeof buf, "{\"id\":%ld,\"ok\":%s,\"state\":\"%s\",\"count\":%d}\n",
                     id, ok ? "true" : "false", state, count);
    else
        n = snprintf(buf, sizeof buf, "{\"id\":%ld,\"ok\":%s,\"state\":\"%s\"}\n",
                     id, ok ? "true" : "false", state);
    if (n <= 0) return;
    DWORD w; WriteFile(h, buf, (DWORD)n, &w, NULL);
    FlushFileBuffers(h);
}

static void ProfileFromPath(const char* path, char* out, size_t cap) {
    out[0] = 0;
    size_t n = strlen(path);
    for (size_t i = n; i >= 8; i--) {   // "profile_" 8 字符
        if (strncmp(path + i - 8, "profile_", 8) == 0) {
            size_t j = i;
            while (path[j] && path[j] >= '0' && path[j] <= '9' && j - i < 10) j++;
            if (j > i) {
                size_t len = 8 + (j - i);              // 含前缀，如 "profile_7"
                if (len >= cap) len = cap - 1;
                memcpy(out, path + i - 8, len); out[len] = 0;
            }
            return;
        }
    }
}

static void HandleLine(HANDLE h, const char* line) {
    long id = 0; json_get_num(line, "id", &id);
    char cmd[16];
    if (!json_get_str(line, "cmd", cmd, sizeof cmd)) { Reply(h, id, 0, "bad-args", -1); return; }

    if (strcmp(cmd, "status") == 0) { Reply(h, id, 1, "ready", -1); return; }
    if (strcmp(cmd, "apply") != 0)  { Reply(h, id, 0, "bad-cmd", -1); return; }

    char path[512];
    if (!json_get_str(line, "path", path, sizeof path)) { Reply(h, id, 0, "bad-args", -1); return; }
    char profile[32]; ProfileFromPath(path, profile, sizeof profile);
    if (!profile[0]) { Reply(h, id, 0, "bad-args", -1); return; }

    Op* op = &g_ops[g_opIdx]; g_opIdx = (g_opIdx + 1) % 4;
    int n = json_get_entries(line, op->ents, MAX_ENTRIES);
    if (n == -2) { Reply(h, id, 0, "bad-entry", -1); return; }
    if (n < 0)    { Reply(h, id, 0, "bad-args", -1); return; }

    strcpy_s(op->profile, _countof(op->profile), profile);
    op->n = n; op->st = 0;
    ResetEvent(g_doneEv);
    InterlockedExchangePointer((PVOID volatile*)&g_pending, op);

    if (WaitForSingleObject(g_doneEv, 2500) != WAIT_OBJECT_0) {
        // 无帧（过场/加载）：取回未执行的操作，无副作用
        Op* back = (Op*)InterlockedExchangePointer((PVOID volatile*)&g_pending, NULL);
        if (back == op) { strcpy_s(op->state, _countof(op->state), "no-frame"); Reply(h, id, 0, "no-frame", -1); return; }
        // executing 中：再等一帧周期余量
        if (WaitForSingleObject(g_doneEv, 3000) != WAIT_OBJECT_0) { Reply(h, id, 0, "apply-timeout", -1); return; }
    }
    if (!op->ok) Log("apply failed %s state=%s", profile, op->state);
    Reply(h, id, op->ok, op->state, op->ok ? op->count : -1);
}

static DWORD WINAPI PipeThread(LPVOID pipeParam) {
    HANDLE pipe = (HANDLE)pipeParam;
    Log("listening \\\\.\\pipe\\DdModToolHotapply (native)");
    for (;;) {
        if (!ConnectNamedPipe(pipe, NULL) && GetLastError() != ERROR_PIPE_CONNECTED) break;
        char buf[16384]; size_t have = 0;
        for (;;) {
            DWORD r = 0;
            if (!ReadFile(pipe, buf + have, (DWORD)(sizeof buf - 1 - have), &r, NULL) || r == 0) break;
            have += r; buf[have] = 0;
            char* nl;
            while ((nl = strchr(buf, '\n')) != NULL) {
                size_t len = (size_t)(nl - buf);
                *nl = 0;
                if (len && buf[len - 1] == '\r') buf[len - 1] = 0;
                if (len) HandleLine(pipe, buf);
                memmove(buf, nl + 1, have - len);
                have -= len + 1;
                buf[have] = 0;
            }
            if (have >= sizeof buf - 1) have = 0;  // 异常大行：丢弃防溢出
        }
        DisconnectNamedPipe(pipe);
    }
    return 0;
}

// ---------------- init ----------------

static DWORD WINAPI InitThread(LPVOID hModule) {
    InitializeCriticalSection(&g_logCS);
    LogOpen();
    Log("init enter");

    HMODULE exe = GetModuleHandleW(NULL);
    g_base = (unsigned char*)exe;
    g_fPush    = (fn_push)(g_base + RVA_VPUSH);
    g_fPersist = (fn_persist)(g_base + RVA_PERSIST);

    IMAGE_DOS_HEADER* dos = (IMAGE_DOS_HEADER*)exe;
    IMAGE_NT_HEADERS* nt = (IMAGE_NT_HEADERS*)(g_base + dos->e_lfanew);
    if (nt->Signature != IMAGE_NT_SIGNATURE) { Log("fatal: bad PE"); goto quit; }

    // 版本校验：FRAME 入口 16 字节模式（只读，无副作用）
    // 先 VirtualQuery 确认页面已提交（小宿主进程里 RVA 越界读会 AV）
    {
        MEMORY_BASIC_INFORMATION mbi;
        SIZE_T need = RVA_FRAME + sizeof FRAME_PATTERN;
        if (!VirtualQuery(g_base + RVA_FRAME, &mbi, sizeof mbi) ||
            mbi.State != MEM_COMMIT ||
            (SIZE_T)((unsigned char*)mbi.BaseAddress + mbi.RegionSize - (g_base + RVA_FRAME)) < need ||
            memcmp(g_base + RVA_FRAME, FRAME_PATTERN, sizeof FRAME_PATTERN) != 0) {
            Log("fatal: frame pattern mismatch (game version changed?)");
            goto quit;
        }
    }

    // 管道先于钩子：句柄被占=别的引擎在服务，干净退出
    HANDLE pipe = CreateNamedPipeW(PIPE_NAME, PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
        PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT, 1, 8192, 8192, 0, NULL);
    if (pipe == INVALID_HANDLE_VALUE) {
        Log("pipe busy (existing engine?) native dll idle-exit");
        goto quit;
    }

    g_doneEv = CreateEventW(NULL, TRUE, FALSE, NULL);   // 手动复位
    if (!g_doneEv || !InstallFrameHook()) { Log("fatal: hook install failed"); CloseHandle(pipe); goto quit; }

    HANDLE hPipeTh = CreateThread(NULL, 0, PipeThread, pipe, 0, NULL);
    if (!hPipeTh) { Log("fatal: no pipe thread"); CloseHandle(pipe); goto quit; }

    Log("attached Darkest.exe pid=%lu (native dll)", GetCurrentProcessId());
    return 0;

quit:
    FreeLibraryAndExitThread((HMODULE)hModule, 1);
    return 0;
}

BOOL APIENTRY DllMain(HMODULE hModule, DWORD reason, LPVOID unused) {
    (void)unused;
    if (reason == DLL_PROCESS_ATTACH) {
        DisableThreadLibraryCalls(hModule);
        CreateThread(NULL, 0, InitThread, (LPVOID)hModule, 0, NULL);
    }
    return TRUE;
}
