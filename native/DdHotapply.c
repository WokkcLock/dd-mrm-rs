// DdHotapply.c v14 —— B 路线引擎（2026-11-16，E1c/E1d/E1e 实证后移植）
//
// 唯一职责：钩 fs_get_file_info（sub_140247F30），对 persist.game* 查询用
// 磁盘真值校正出参 out+0x440 的 size 字段。
//
// 机制（三轮 Frida 实验定案）：
//   - 游戏侧 size 缓存按会话长寿命（非单条目），外部工具改写 persist.game.json
//     后，同会话内的游戏查询会拿到陈旧 size → 载入路径按旧 size 分配缓冲 →
//     字符串长度断言（heirarchical_file.cpp:718）崩溃。
//   - 校正时机 = 原函数返回后（onLeave 语义）：缓存命中与未命中两条路径都
//     覆盖；cur=0 是 found=0 探测（Documents 假路径），不碰。
//   - 磁盘真值优先走游戏自己的逻辑路径（进程内 CreateFileW 被 Steam
//     Auto-Cloud 重定向）；失败再回退 remoteRoot/profile_N/<basename>
//     （remoteRoot 由 UI 经共享内存下发 = 用户配置的存档根；不硬编码，
//     没给或失配 → 回退禁用 → 不校正不崩溃，安全退化）。
//
// 与旧版（v13 管道+帧钩+堆搜索）彻底切割：无管道、无帧钩、无屏对象搜索、
// 无游戏函数调用。注入即自活，日志即证据（E2 = DLL 日志 + 肉眼）。
#include <windows.h>
#include <stdio.h>
#include <stdarg.h>
#include <stdint.h>
#include <string.h>

// ---------------- 配置块 v8（与 crates/hotapply/src/injector.rs 一一对应） ----------------
#define CFG_MAPNAME            L"Local\\DdModToolHotapplyCfg"
#define CFG_MAGIC_V2           0x5A4D4444u
#define CFG_VERSION_V2         8u
#define CFG_FLAG_HAS_ANCHORS   1u
#define CFG_MAP_SIZE           1024
#define CFG_REMOTE_MAX_CHARS   256          // remoteRoot 容量（wchar）
#define CFG_OFF_MAGIC          0
#define CFG_OFF_VERSION        4
#define CFG_OFF_FLAGS          8
#define CFG_OFF_FSVA           12           // u32 rva
#define CFG_OFF_PATTERN        16           // 16 字节入口指纹
#define CFG_OFF_REMOTEROOT     32           // UTF-16，NUL 结尾，512 字节
#define CFG_OFF_LOGDIR         544          // UTF-16，NUL 结尾，480 字节

// ---- 锚点默认值（= 2026-11-05 版 1c67048e，exe 全体唯一实证 raw=1）。
//      配置块 HAS_ANCHORS 时被覆盖；指纹校验失败 → 不装钩。----
#define RVA_FSGET_DFLT 0x247F30
static const unsigned char FS_PATTERN_DFLT[16] = {
    0x48,0x89,0x5C,0x24,0x18,0x55,0x56,0x57,0x41,0x54,0x41,0x55,0x41,0x56,0x41,0x57 };

// ---- 结构偏移（E1d IDA 实证）----
#define OFF_NAME_STR   0x20      // nameBox+0x20 = 名字 char[]（"steam_profile_N" / "profile_N"）
#define OFF_OUT_SIZE   0x440     // out+0x440 = u32 文件大小（0=不存在）
#define BODY_CHECK_LEN 0x300     // 体校验窗口：函数头之后含 0x11B8/0x15B8 位移

// ---- 钩几何（0x247F30 头 14B 恰完整指令边界：4+1+1+1+2+2+3）----
#define FS_PATCH_LEN   14        // 蹦床复制的原指令字节数（push r14 收尾）
#define JMP_PATCH_LEN  13        // mov r11,imm64(10B) + jmp r11(3B)，余 1B nop 填充

#define DLL_BUILD_TAG "2026-11-16a fsfix"

static DWORD g_rvaFs = RVA_FSGET_DFLT;
static unsigned char g_fsPattern[16];
static int g_anchorsFromCfg = 0;
static wchar_t g_remoteRoot[CFG_REMOTE_MAX_CHARS] = L"";   // 空 = 回退禁用（用户拍板：不硬编码）

static unsigned char* g_base = NULL;
static void* g_trampoline = NULL;        // RWX 蹦床（fsfix_thunk 经指针调用）
void* g_trampoline_ptr = NULL;           // thunk64.asm EXTERN 引用的就是它

static CRITICAL_SECTION g_logCS;
static wchar_t g_logDir[MAX_PATH] = L"";
static HANDLE g_logFile = INVALID_HANDLE_VALUE;
static volatile LONG g_queries = 0;      // persist.game* 查询计数（日志自证）
static volatile LONG g_fixes = 0;

static unsigned u32le(const unsigned char* p) { return p[0] | p[1]<<8 | p[2]<<16 | (unsigned)p[3]<<24; }

// ---------------- 日志（沿 v13 件，追加写 engine.log） ----------------
static void LogOpen(void) {
    HANDLE hMap = OpenFileMappingW(FILE_MAP_READ, FALSE, CFG_MAPNAME);
    if (hMap) {
        unsigned char* p = (unsigned char*)MapViewOfFile(hMap, FILE_MAP_READ, 0, 0, 0);
        if (p) {
            if (u32le(p + CFG_OFF_MAGIC) == CFG_MAGIC_V2 && u32le(p + CFG_OFF_VERSION) == CFG_VERSION_V2) {
                if (u32le(p + CFG_OFF_FLAGS) & CFG_FLAG_HAS_ANCHORS) {
                    g_rvaFs = u32le(p + CFG_OFF_FSVA);
                    memcpy(g_fsPattern, p + CFG_OFF_PATTERN, 16);
                    g_anchorsFromCfg = 1;
                }
                wcsncpy_s(g_remoteRoot, CFG_REMOTE_MAX_CHARS,
                          (wchar_t*)(p + CFG_OFF_REMOTEROOT), CFG_REMOTE_MAX_CHARS - 1);
                wcsncpy_s(g_logDir, MAX_PATH,
                          (wchar_t*)(p + CFG_OFF_LOGDIR), (CFG_MAP_SIZE - CFG_OFF_LOGDIR) / 2);
            }
            UnmapViewOfFile(p);
        }
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
        CreateDirectoryW(g_logDir, NULL);
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

// ---------------- 磁盘真值 ----------------
static long long FileSizeW(const wchar_t* wpath) {
    HANDLE h = CreateFileW(wpath, GENERIC_READ,
                           FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                           NULL, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, NULL);
    if (h == INVALID_HANDLE_VALUE) return -1;
    LARGE_INTEGER sz;
    BOOL ok = GetFileSizeEx(h, &sz);
    CloseHandle(h);
    return ok ? sz.QuadPart : -1;
}

static int ToWide(const char* s, wchar_t* dst, int cap) {
    int n = MultiByteToWideChar(CP_UTF8, 0, s, -1, dst, cap - 1);
    if (n <= 0) { if (cap > 0) dst[0] = 0; return 0; }
    dst[n] = 0;   // MultiByte...含 NUL 计数；确保截断也终结
    return 1;
}

// 磁盘真值：① 游戏逻辑路径（进程内走 Steam Auto-Cloud 重定向）
//           ② remoteRoot/profile_N/<basename>（UI 下发的存档根；空 = 禁用）
static long long DiskTruthW(const char* path, const void* nameBox) {
    wchar_t w[MAX_PATH];
    if (!ToWide(path, w, MAX_PATH)) return -1;
    long long t = FileSizeW(w);
    if (t >= 0) return t;

    if (!g_remoteRoot[0] || !nameBox) return -1;
    const char* name = (const char*)nameBox + OFF_NAME_STR;
    const char* pp = strstr(name, "profile_");
    if (!pp) return -1;
    pp += 8;
    unsigned n = 0; int any = 0;
    while (*pp >= '0' && *pp <= '9') { n = n * 10u + (unsigned)(*pp - '0'); pp++; any = 1; }
    if (!any || n > 99) return -1;

    const char* last = NULL;
    for (const char* p = path; *p; p++) if (*p == '\\' || *p == '/') last = p;
    const char* base = last ? last + 1 : path;
    wchar_t wb[MAX_PATH];
    if (!ToWide(base, wb, MAX_PATH)) return -1;
    wchar_t full[MAX_PATH];
    if (_snwprintf_s(full, MAX_PATH, _TRUNCATE, L"%s\\profile_%u\\%s", g_remoteRoot, n, wb) < 0)
        return -1;
    return FileSizeW(full);
}

// ---------------- 钩 handler（蹦床跑完原函数后由 fsfix_thunk 调用） ----------------
// 签名：fsfix_op(nameBox, path, out)。v5 脚本 1:1 移植 + 日志。
// 门控：cur>0（排除 found=0 探测）、t>0（磁盘拿不到真值就不碰）、t != cur。
void fsfix_op(const void* nameBox, const char* path, unsigned char* out) {
    if (!path || !out) return;
    if (!strstr(path, "persist.game")) return;
    InterlockedIncrement(&g_queries);

    unsigned cur = u32le(out + OFF_OUT_SIZE);
    long long t = DiskTruthW(path, nameBox);
    if (t <= 0 || cur == 0 || (unsigned long long)t == cur) {
        // 只记前若干条，避免日志风暴（探测/一致是常态）
        if (g_queries < 40 || (g_queries % 100) == 0)
            Log("q #%d keep path=%hs cur=%u truth=%lld", g_queries, path, cur, t);
        return;
    }
    unsigned fixed = (unsigned)t;
    memcpy(out + OFF_OUT_SIZE, &fixed, 4);
    InterlockedIncrement(&g_fixes);
    Log("FIX #%d path=%hs cur=%u -> %u", g_fixes, path, cur, fixed);
}

// ---------------- 装钩（InstallHook13 参数化，唯一调用点 fs_get_file_info） ----------------
static int InstallHookN(unsigned char* target, void* thunk, int len) {
    // 蹦床：len 字节原指令 + mov r11,(target+len) + jmp r11
    g_trampoline = VirtualAlloc(NULL, 64, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE);
    if (!g_trampoline) return 0;
    unsigned char* tr = (unsigned char*)g_trampoline;
    memcpy(tr, target, (size_t)len);
    tr[len]     = 0x49; tr[len + 1] = 0xBB;                    // mov r11, imm64
    uint64_t back = (uint64_t)(target + len);
    memcpy(tr + len + 2, &back, 8);
    tr[len + 10] = 0x41; tr[len + 11] = 0xFF; tr[len + 12] = 0xE3; // jmp r11

    // 补丁：mov r11, thunk(10B) + jmp r11(3B) + nop 填充到 len
    unsigned char patch[JMP_PATCH_LEN + 1];
    patch[0] = 0x49; patch[1] = 0xBB;
    uint64_t th = (uint64_t)thunk;
    memcpy(patch + 2, &th, 8);
    patch[10] = 0x41; patch[11] = 0xFF; patch[12] = 0xE3;
    if (len > JMP_PATCH_LEN) patch[JMP_PATCH_LEN] = 0x90;

    DWORD old;
    if (!VirtualProtect(target, (SIZE_T)len, PAGE_EXECUTE_READWRITE, &old)) return 0;
    memcpy(target, patch, (size_t)len);
    VirtualProtect(target, (SIZE_T)len, old, &old);
    FlushInstructionCache(GetCurrentProcess(), target, (SIZE_T)len);
    g_trampoline_ptr = g_trampoline;        // 发布在补丁写入之后
    return 1;
}

// ---------------- 初始化线程（DllMain 只 CreateThread，纪律） ----------------
static DWORD WINAPI InitThread(void* ctx) {
    (void)ctx;
    InitializeCriticalSection(&g_logCS);
    if (!g_anchorsFromCfg) memcpy(g_fsPattern, FS_PATTERN_DFLT, 16);
    LogOpen();

    g_base = (unsigned char*)GetModuleHandleW(NULL);
    Log("v14 fsfix ready build=%s base=%p rva=%lx src=%s remoteRoot=%ls",
        DLL_BUILD_TAG, (void*)g_base, g_rvaFs,
        g_anchorsFromCfg ? "cfg" : "default",
        g_remoteRoot[0] ? g_remoteRoot : L"(disabled)");

    unsigned char* target = g_base + g_rvaFs;
    if (memcmp(target, g_fsPattern, 16) != 0) {
        Log("ABORT: pattern mismatch @%p (game updated?) — hook NOT installed", (void*)target);
        return 0;
    }
    // 体校验（双保险）：头 0x300 字节内须有 0x11B8/0x15B8 缓存位移
    int hasA = 0, hasB = 0;
    for (int i = 0; i + 4 < BODY_CHECK_LEN; i += 1) {
        if (target[i]   == 0xB8 && target[i+1] == 0x11 && target[i+2] == 0x00 && target[i+3] == 0x00) hasA = 1;
        if (target[i]   == 0xB8 && target[i+1] == 0x15 && target[i+2] == 0x00 && target[i+3] == 0x00) hasB = 1;
    }
    if (!hasA || !hasB) {
        Log("ABORT: body displacement check failed (%d,%d) — hook NOT installed", hasA, hasB);
        return 0;
    }

    extern void* fsfix_thunk(void);   // 汇编导出
    if (!InstallHookN(target, (void*)&fsfix_thunk, FS_PATCH_LEN)) {
        Log("ABORT: InstallHookN failed gle=%lu", GetLastError());
        return 0;
    }
    Log("hook installed @%p (len=%d) trampoline=%p", (void*)target, FS_PATCH_LEN, g_trampoline);
    return 0;
}

BOOL WINAPI DllMain(HINSTANCE hDll, DWORD reason, LPVOID reserved) {
    (void)reserved;
    if (reason == DLL_PROCESS_ATTACH) {
        DisableThreadLibraryCalls(hDll);
        HANDLE h = CreateThread(NULL, 0, InitThread, NULL, 0, NULL);
        if (h) CloseHandle(h);
    }
    return TRUE;
}
