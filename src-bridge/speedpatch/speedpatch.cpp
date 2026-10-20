/*
 * OpenSpeedy - Open Source Game Speed Controller
 * Copyright (C) 2025 Game1024
 *
 * This program is free software: you can redistribute it
 * and/or modify it under the terms of the GNU General
 * Public License as published by the Free Software
 * Foundation, either version 3 of the License, or (at your
 * option) any later version.
 *
 * This program is distributed in the hope that it will be
 * useful, but WITHOUT ANY WARRANTY; without even the
 * implied warranty of MERCHANTABILITY or FITNESS FOR A
 * PARTICULAR PURPOSE.  See the GNU General Public License
 * for more details.
 *
 * You should have received a copy of the GNU General Public
 * License along with this program.  If not, see
 * <https://www.gnu.org/licenses/>.
 */
#include <windows.h>
#include <winternl.h>
#include "Minhook.h"
#include "speedpatch.h"
#include <atomic>
#include <mmsystem.h>
#include <shared_mutex>
#include <mutex>
#include <cmath>
#pragma comment(lib, "winmm.lib")
#pragma comment(lib, "user32.lib")
#pragma data_seg("shared")
static std::atomic<double> factor = 1.0;
// 安装失败的 hook 数。放在共享段里 —— 注入到游戏里的那份 DLL 记录到的失败
// 也能被 bridge 进程读到（两者映射同一个 DLL 镜像），从而写进 bridge 的日志。
static std::atomic<unsigned> hookFailures = 0;
#pragma data_seg()
#pragma comment(linker, "/section:shared,RWS")

static std::shared_mutex mutex;
static HANDLE hFileMap;
static bool*  pEnabled;

SPEEDPATCH_API void SP_SetSpeed(double factor_)
{
    // 拒绝 0、负数、NaN、无穷大 —— 这些值会让时间停止或倒退，游戏直接卡死
    if (!(factor_ > 0.0) || !std::isfinite(factor_))
    {
        return;
    }
    factor.store(factor_);
}

SPEEDPATCH_API double SP_GetSpeed()
{
    return factor.load();
}

SPEEDPATCH_API unsigned SP_GetHookFailures()
{
    return hookFailures.load();
}

void SP_Install()
{
    DWORD processId = GetCurrentProcessId();
    WCHAR filemapName[64];
    GetProcessFileMapName(processId, filemapName, 64);
    hFileMap = CreateFileMapping(INVALID_HANDLE_VALUE, NULL, PAGE_READWRITE, 0, sizeof (bool), filemapName);
    if (hFileMap == NULL)
    {
        return;
    }
    pEnabled = (bool*) MapViewOfFile(hFileMap, FILE_MAP_ALL_ACCESS, 0, 0, sizeof (bool));
    if (pEnabled == NULL)
    {
        CloseHandle(hFileMap);
        hFileMap = NULL;
        return;
    }
    *pEnabled = true;
}

void SP_Uninstall()
{
    if (hFileMap != NULL)
    {
        UnmapViewOfFile(pEnabled);
        CloseHandle(hFileMap);
    }
}

BOOL SP_IsEnabled()
{
    return pEnabled ? *pEnabled : FALSE;
}

SPEEDPATCH_API BOOL SP_IsEnabledById(DWORD processId)
{
    WCHAR filemapName[64];
    GetProcessFileMapName(processId, filemapName, 64);
    HANDLE hFileMap_ = OpenFileMapping(FILE_MAP_READ, FALSE, filemapName);
    if (hFileMap_ == NULL)
    {
        return FALSE;
    }
    bool* pStatus = (bool*) MapViewOfFile(hFileMap_, FILE_MAP_READ, 0, 0, sizeof (bool));
    if (pStatus == NULL)
    {
        CloseHandle(hFileMap_);
        return FALSE;
    }
    BOOL enabled = (*pStatus) ? TRUE : FALSE;
    UnmapViewOfFile(pStatus);
    CloseHandle(hFileMap_);
    return enabled;
}

void SP_Enable(DWORD processId)
{
    WCHAR filemapName[64];
    GetProcessFileMapName(processId, filemapName, 64);
    HANDLE hFileMap_ = OpenFileMapping(FILE_MAP_ALL_ACCESS, FALSE, filemapName);
    if (hFileMap_ == NULL)
    {
        return;
    }
    bool* pStatus = (bool*) MapViewOfFile(hFileMap_, FILE_MAP_ALL_ACCESS, 0, 0, sizeof (bool));
    if (pStatus == NULL)
    {
        CloseHandle(hFileMap_);
        return;
    }
    *pStatus = true;
    UnmapViewOfFile(pStatus);
    CloseHandle(hFileMap_);
}

void SP_Disable(DWORD processId)
{
    WCHAR filemapName[64];
    GetProcessFileMapName(processId, filemapName, 64);
    HANDLE hFileMap_ = OpenFileMapping(FILE_MAP_ALL_ACCESS, FALSE, filemapName);
    if (hFileMap_ == NULL)
    {
        return;
    }
    bool* pStatus = (bool*) MapViewOfFile(hFileMap_, FILE_MAP_ALL_ACCESS, 0, 0, sizeof (bool));
    if (pStatus == NULL)
    {
        CloseHandle(hFileMap_);
        return;
    }
    *pStatus = false;
    UnmapViewOfFile(pStatus);
    CloseHandle(hFileMap_);
}

VOID GetProcessFileMapName(DWORD processId, WCHAR* buf, DWORD bufSize)
{
    wsprintfW(buf, L"OpenSpeedy.%lu", processId);
}

double SpeedFactor()
{
    if (SP_IsEnabled())
    {
        double f = factor.load();
        // 防呆：倍率非正数或 NaN 时当作 1.0，避免除以 0 / 时间倒退
        return (f > 0.0) ? f : 1.0;
    }
    else
    {
        return 1.0;
    }
}

// ── 等待时间缩放 ─────────────────────────────────────────────────────────
// 原版直接 dwMilliseconds / SpeedFactor()：
//   * Sleep/SleepEx 没有排除 INFINITE —— 倍率 < 1 时结果超出 DWORD 范围（未定义行为，
//     可能变成 0），原本应该永远等待的线程会立刻返回。
//   * 大数值也可能溢出。
// 这里统一处理：0 与 INFINITE 原样传递，其余按倍率缩放并夹在合法范围内。
static DWORD ScaleTimeout(DWORD ms)
{
    if (ms == 0 || ms == INFINITE)
    {
        return ms;
    }
    double v = (double) ms / SpeedFactor();
    if (v >= (double) (INFINITE - 1))
    {
        return INFINITE - 1;
    }
    return (DWORD) v;
}

// 同上，但结果至少为 1（timeSetEvent 的 uDelay 为 0 会直接失败、返回 NULL，
// 导致游戏的多媒体计时器建立失败）。
static UINT ScaleDelayMin1(UINT ms)
{
    if (ms == 0)
    {
        return 0;
    }
    DWORD v = ScaleTimeout(ms);
    return v == 0 ? 1 : v;
}

// ── 线程安全的缩放时钟 ───────────────────────────────────────────────────
// 原版每个时间函数用 4 个独立的 atomic（base/last × real/hook），倍率改变时由
// 任意线程把 last 复制成 base。多线程下这几个值不是成对更新的：一个线程刚写了
// lastReal、还没写 lastHook，另一个线程就拿它们当新基准 —— 游戏拿到的时间会
// 突然往前跳或往回退。时间倒退会让很多游戏卡住或“加速失效”。
//
// 新做法：每个时钟一把 SRW 锁，在锁内读取真实时间，并且每次调用都以上一次的
// 结果为基准累加：hook += factor × (now - lastReal)。小数部分累计在 frac 里，
// 不会因为取整而越跑越慢。结果单调、可正确处理计数器回绕，倍率改变立即生效。
template <typename T, typename D>
struct ScaledClock
{
    SRWLOCK lock = SRWLOCK_INIT;
    bool    init = false;
    T       lastReal{};
    T       lastHook{};
    double  frac = 0.0;

    template <typename F>
    T Now(F readReal)
    {
        AcquireSRWLockExclusive(&lock);
        T now = readReal();
        if (!init)
        {
            lastReal = now;
            lastHook = now;
            frac     = 0.0;
            init     = true;
        }
        // 用有号差值：GetMessageTime 返回的是“最后一条消息的时间”，可能比上次小
        D elapsed     = (D) (now - lastReal);
        double scaled = SpeedFactor() * (double) elapsed + frac;
        double whole  = (scaled >= 0.0) ? floor(scaled) : ceil(scaled);
        frac          = scaled - whole;
        lastHook      = (T) (lastHook + (T) (D) whole);
        lastReal      = now;
        T result      = lastHook;
        ReleaseSRWLockExclusive(&lock);
        return result;
    }
};

static ScaledClock<DWORD, LONG>         clk_TimeGetTime;
static ScaledClock<DWORD, LONG>         clk_GetMessageTime;
static ScaledClock<DWORD, LONG>         clk_GetTickCount;
static ScaledClock<ULONGLONG, LONGLONG> clk_GetTickCount64;
static ScaledClock<ULONGLONG, LONGLONG> clk_QueryPerformanceCounter;
static ScaledClock<ULONGLONG, LONGLONG> clk_GetSystemTimeAsFileTime;
static ScaledClock<ULONGLONG, LONGLONG> clk_GetSystemTimePreciseAsFileTime;

static inline ULONGLONG FileTimeToU64(const FILETIME& ft)
{
    return ((ULONGLONG) ft.dwHighDateTime << 32) | ft.dwLowDateTime;
}

static inline FILETIME U64ToFileTime(ULONGLONG v)
{
    FILETIME ft;
    ft.dwLowDateTime  = (DWORD) (v & 0xFFFFFFFFULL);
    ft.dwHighDateTime = (DWORD) (v >> 32);
    return ft;
}

// ── 等待 / 计时器类 ──────────────────────────────────────────────────────

typedef VOID (WINAPI* SLEEP) (DWORD);

static SLEEP Real_Sleep = NULL;

VOID WINAPI Hook_Sleep(DWORD dwMilliseconds)
{
    Real_Sleep(ScaleTimeout(dwMilliseconds));
}

typedef DWORD (WINAPI* SLEEPEX) (DWORD, BOOL);

static SLEEPEX Real_SleepEx = NULL;

DWORD WINAPI Hook_SleepEx(DWORD dwMilliseconds, BOOL bAlertable)
{
    return Real_SleepEx(ScaleTimeout(dwMilliseconds), bAlertable);
}

typedef DWORD (WINAPI* WAITFORSINGLEOBJECT) (HANDLE, DWORD);

static WAITFORSINGLEOBJECT Real_WaitForSingleObject = NULL;

DWORD WINAPI Hook_WaitForSingleObject(HANDLE hHandle, DWORD dwMilliseconds)
{
    return Real_WaitForSingleObject(hHandle, ScaleTimeout(dwMilliseconds));
}

typedef DWORD (WINAPI* WAITFORSINGLEOBJECTEX) (HANDLE, DWORD, BOOL);

static WAITFORSINGLEOBJECTEX Real_WaitForSingleObjectEx = NULL;

DWORD WINAPI Hook_WaitForSingleObjectEx(HANDLE hHandle, DWORD dwMilliseconds, BOOL bAlertable)
{
    return Real_WaitForSingleObjectEx(hHandle, ScaleTimeout(dwMilliseconds), bAlertable);
}

typedef DWORD (WINAPI* WAITFORMULTIPLEOBJECTS) (DWORD, const HANDLE*, BOOL, DWORD);

static WAITFORMULTIPLEOBJECTS Real_WaitForMultipleObjects = NULL;

DWORD WINAPI Hook_WaitForMultipleObjects(DWORD nCount, const HANDLE* lpHandles, BOOL bWaitAll, DWORD dwMilliseconds)
{
    return Real_WaitForMultipleObjects(nCount, lpHandles, bWaitAll, ScaleTimeout(dwMilliseconds));
}

typedef DWORD (WINAPI* WAITFORMULTIPLEOBJECTSEX) (DWORD, const HANDLE*, BOOL, DWORD, BOOL);

static WAITFORMULTIPLEOBJECTSEX Real_WaitForMultipleObjectsEx = NULL;

DWORD WINAPI Hook_WaitForMultipleObjectsEx(DWORD nCount, const HANDLE* lpHandles, BOOL bWaitAll, DWORD dwMilliseconds, BOOL bAlertable)
{
    return Real_WaitForMultipleObjectsEx(nCount, lpHandles, bWaitAll, ScaleTimeout(dwMilliseconds), bAlertable);
}

// 新增：很多游戏的主循环用 MsgWaitForMultipleObjects(Ex) 一边处理窗口消息一边
// 等待下一帧。原版没有 hook 它，这类游戏的帧等待不会跟着倍率缩短。
typedef DWORD (WINAPI* MSGWAITFORMULTIPLEOBJECTS) (DWORD, const HANDLE*, BOOL, DWORD, DWORD);

static MSGWAITFORMULTIPLEOBJECTS Real_MsgWaitForMultipleObjects = NULL;

DWORD WINAPI Hook_MsgWaitForMultipleObjects(DWORD nCount, const HANDLE* pHandles, BOOL fWaitAll, DWORD dwMilliseconds, DWORD dwWakeMask)
{
    return Real_MsgWaitForMultipleObjects(nCount, pHandles, fWaitAll, ScaleTimeout(dwMilliseconds), dwWakeMask);
}

typedef DWORD (WINAPI* MSGWAITFORMULTIPLEOBJECTSEX) (DWORD, const HANDLE*, DWORD, DWORD, DWORD);

static MSGWAITFORMULTIPLEOBJECTSEX Real_MsgWaitForMultipleObjectsEx = NULL;

DWORD WINAPI Hook_MsgWaitForMultipleObjectsEx(DWORD nCount, const HANDLE* pHandles, DWORD dwMilliseconds, DWORD dwWakeMask, DWORD dwFlags)
{
    return Real_MsgWaitForMultipleObjectsEx(nCount, pHandles, ScaleTimeout(dwMilliseconds), dwWakeMask, dwFlags);
}

typedef UINT_PTR (WINAPI* SETTIMER) (HWND, UINT_PTR, UINT, TIMERPROC);

static SETTIMER Real_SetTimer = NULL;

UINT_PTR WINAPI Hook_SetTimer(HWND hWnd, UINT_PTR nIDEvent, UINT uElapse, TIMERPROC lpTimerFunc)
{
    // SetTimer 接受 0（会被提升到 USER_TIMER_MINIMUM），上限 USER_TIMER_MAXIMUM
    DWORD v = ScaleTimeout(uElapse);
    if (v > USER_TIMER_MAXIMUM)
    {
        v = USER_TIMER_MAXIMUM;
    }
    return Real_SetTimer(hWnd, nIDEvent, v, lpTimerFunc);
}

typedef MMRESULT (WINAPI* TIMESETEVENT) (UINT, UINT, LPTIMECALLBACK, DWORD_PTR, UINT);

static TIMESETEVENT Real_TimeSetEvent = NULL;

MMRESULT WINAPI Hook_TimeSetEvent(UINT uDelay, UINT uResolution, LPTIMECALLBACK lpTimeProc, DWORD_PTR dwUser, UINT fuEvent)
{
    // 原版在倍率大于 uDelay 时会算出 0，timeSetEvent(0, …) 直接失败。
    return Real_TimeSetEvent(ScaleDelayMin1(uDelay), uResolution, lpTimeProc, dwUser, fuEvent);
}

// SetWaitableTimer：
//   * dueTime 为负数 = 相对时间（100ns 单位），可以缩放；
//   * dueTime 为正数 = 绝对时间（FILETIME），原版把它也除以倍率，结果变成几百年前
//     （倍率 > 1，立刻触发）或几百年后（倍率 < 1，永远不触发）。现在保持原样。
//   * lPeriod（周期，毫秒）原版没缩放，周期性计时器完全不受加速影响。现在一起缩放。
static LARGE_INTEGER ScaleDueTime(const LARGE_INTEGER* lpDueTime)
{
    LARGE_INTEGER dueTime = *lpDueTime;
    if (dueTime.QuadPart < 0)
    {
        double v = (double) dueTime.QuadPart / SpeedFactor();
        dueTime.QuadPart = (LONGLONG) v;
        if (dueTime.QuadPart == 0)
        {
            dueTime.QuadPart = -1; // 仍然是相对时间，避免变成“绝对时间 0”
        }
    }
    return dueTime;
}

static LONG ScalePeriod(LONG lPeriod)
{
    if (lPeriod <= 0)
    {
        return lPeriod;
    }
    DWORD v = ScaleTimeout((DWORD) lPeriod);
    if (v == 0)
    {
        v = 1; // 0 表示“只触发一次”，不能让周期计时器变成单次
    }
    if (v > 0x7FFFFFFF)
    {
        v = 0x7FFFFFFF;
    }
    return (LONG) v;
}

typedef BOOL (WINAPI* SETWAITABLETIMER) (HANDLE, const LARGE_INTEGER*, LONG, PTIMERAPCROUTINE, LPVOID, BOOL);

static SETWAITABLETIMER Real_SetWaitableTimer = NULL;

BOOL WINAPI Hook_SetWaitableTimer(HANDLE hTimer, const LARGE_INTEGER* lpDueTime, LONG lPeriod, PTIMERAPCROUTINE pfnCompletionRoutine, LPVOID lpArgToCompletionRoutine, BOOL fResume)
{
    if (lpDueTime == NULL)
    {
        return Real_SetWaitableTimer(hTimer, lpDueTime, lPeriod, pfnCompletionRoutine, lpArgToCompletionRoutine, fResume);
    }
    LARGE_INTEGER dueTime = ScaleDueTime(lpDueTime);
    return Real_SetWaitableTimer(hTimer, &dueTime, ScalePeriod(lPeriod), pfnCompletionRoutine, lpArgToCompletionRoutine, fResume);
}

typedef BOOL (WINAPI* SETWAITABLETIMEREX) (HANDLE, const LARGE_INTEGER*, LONG, PTIMERAPCROUTINE, LPVOID, PREASON_CONTEXT, ULONG);

static SETWAITABLETIMEREX Real_SetWaitableTimerEx = NULL;

BOOL WINAPI Hook_SetWaitableTimerEx(HANDLE hTimer, const LARGE_INTEGER* lpDueTime, LONG lPeriod, PTIMERAPCROUTINE pfnCompletionRoutine, LPVOID lpArgToCompletionRoutine, PREASON_CONTEXT WakeContext, ULONG TolerableDelay)
{
    if (lpDueTime == NULL)
    {
        return Real_SetWaitableTimerEx(hTimer, lpDueTime, lPeriod, pfnCompletionRoutine, lpArgToCompletionRoutine, WakeContext, TolerableDelay);
    }
    LARGE_INTEGER dueTime = ScaleDueTime(lpDueTime);
    return Real_SetWaitableTimerEx(hTimer, &dueTime, ScalePeriod(lPeriod), pfnCompletionRoutine, lpArgToCompletionRoutine, WakeContext, TolerableDelay);
}

// ── 时间读取类 ───────────────────────────────────────────────────────────

typedef DWORD (WINAPI* TIMEGETTIME) (VOID);

static TIMEGETTIME Real_TimeGetTime = NULL;

DWORD WINAPI Hook_TimeGetTime(VOID)
{
    return clk_TimeGetTime.Now([] { return Real_TimeGetTime(); });
}

typedef LONG (WINAPI* GETMESSAGETIME) (VOID);

static GETMESSAGETIME Real_GetMessageTime = NULL;

LONG WINAPI Hook_GetMessageTime(VOID)
{
    return (LONG) clk_GetMessageTime.Now([] { return (DWORD) Real_GetMessageTime(); });
}

typedef DWORD (WINAPI* GETTICKCOUNT) (VOID);

static GETTICKCOUNT Real_GetTickCount = NULL;

DWORD WINAPI Hook_GetTickCount(VOID)
{
    return clk_GetTickCount.Now([] { return Real_GetTickCount(); });
}

typedef ULONGLONG (WINAPI* GETTICKCOUNT64) (VOID);

static GETTICKCOUNT64 Real_GetTickCount64 = NULL;

ULONGLONG WINAPI Hook_GetTickCount64(VOID)
{
    return clk_GetTickCount64.Now([] { return Real_GetTickCount64(); });
}

typedef BOOL (WINAPI* QUERYPERFORMANCECOUNTER) (LARGE_INTEGER*);

static QUERYPERFORMANCECOUNTER Real_QueryPerformanceCounter = NULL;

BOOL WINAPI Hook_QueryPerformanceCounter(LARGE_INTEGER* lpPerformanceCount)
{
    if (lpPerformanceCount == NULL)
    {
        return Real_QueryPerformanceCounter(lpPerformanceCount);
    }
    BOOL rtncode = TRUE;
    ULONGLONG v = clk_QueryPerformanceCounter.Now([&rtncode] {
        LARGE_INTEGER now;
        rtncode = Real_QueryPerformanceCounter(&now);
        return (ULONGLONG) now.QuadPart;
    });
    lpPerformanceCount->QuadPart = (LONGLONG) v;
    return rtncode;
}

typedef VOID (WINAPI* GETSYSTEMTIMEASFILETIME) (LPFILETIME);

static GETSYSTEMTIMEASFILETIME Real_GetSystemTimeAsFileTime = NULL;

VOID WINAPI Hook_GetSystemTimeAsFileTime(LPFILETIME lpSystemTimeAsFileTime)
{
    if (lpSystemTimeAsFileTime == NULL)
    {
        return;
    }
    ULONGLONG v = clk_GetSystemTimeAsFileTime.Now([] {
        FILETIME ft = { 0, 0 };
        Real_GetSystemTimeAsFileTime(&ft);
        return FileTimeToU64(ft);
    });
    *lpSystemTimeAsFileTime = U64ToFileTime(v);
}

typedef VOID (WINAPI* GETSYSTEMTIMEPRECISEASFILETIME) (LPFILETIME);

static GETSYSTEMTIMEPRECISEASFILETIME Real_GetSystemTimePreciseAsFileTime = NULL;

VOID WINAPI Hook_GetSystemTimePreciseAsFileTime(LPFILETIME lpSystemTimeAsFileTime)
{
    if (lpSystemTimeAsFileTime == NULL)
    {
        return;
    }
    ULONGLONG v = clk_GetSystemTimePreciseAsFileTime.Now([] {
        FILETIME ft = { 0, 0 };
        Real_GetSystemTimePreciseAsFileTime(&ft);
        return FileTimeToU64(ft);
    });
    *lpSystemTimeAsFileTime = U64ToFileTime(v);
}

// 记录 hook 安装失败。DllMain 运行在加载器锁里：既不能弹 MessageBox（模态框会把
// 目标进程和 bridge 一起卡死），也不能写文件（同样在加载器锁里）。输出到调试器，
// 并累加到共享计数器，由 bridge 读出来写进自己的日志。
static void ReportHookFailure(const char* stage, MH_STATUS status)
{
    char buf[160];
    wsprintfA(buf, "[speedpatch] %s failed (status %d) — hook skipped\n", stage, (int)status);
    OutputDebugStringA(buf);
    hookFailures.fetch_add(1);
}

template <typename S, typename T>
inline BOOL MH_HOOK(S* pTarget, S* pDetour, T** ppOriginal)
{
    MH_STATUS status = MH_CreateHook(reinterpret_cast<LPVOID> (pTarget), reinterpret_cast<LPVOID> (pDetour), reinterpret_cast<LPVOID*> (ppOriginal));
    if (status != MH_OK)
    {
        // 这一步失败时目标函数一个字节都没动，detour 不可能被调用，
        // ppOriginal 也保持 MinHook 写下的值 —— 不要去改它。
        ReportHookFailure("MH_CreateHook", status);
        return FALSE;
    }

    status = MH_EnableHook(reinterpret_cast<LPVOID> (pTarget));
    if (status != MH_OK)
    {
        // MinHook 在写入前会先 VirtualProtect 校验，失败时目标函数保持原样。
        // 所以这里故意不删 hook：留着已建好的 trampoline 比删掉更安全 ——
        // 即使目标真被改了一半，trampoline 仍指向正确的原始代码。
        ReportHookFailure("MH_EnableHook", status);
        return FALSE;
    }

    return TRUE;
}

template <typename T>
VOID MH_UNHOOK(T* pTarget)
{
    MH_RemoveHook(reinterpret_cast<LPVOID> (pTarget));
}

BOOL APIENTRY DllMain(HMODULE hModule, DWORD ul_reason_for_call, LPVOID lpReserved)
{
    switch (ul_reason_for_call)
    {
    case DLL_PROCESS_ATTACH:

        if (MH_Initialize() != MH_OK)
        {
            // 不能弹 MessageBox —— DllMain 持有加载器锁。返回 FALSE 让这次加载干净失败。
            OutputDebugStringA("[speedpatch] MH_Initialize failed — DLL not loaded\n");
            return FALSE;
        }
        SP_Install();
        // 各个时钟在第一次被调用时以当时的真实时间初始化（见 ScaledClock），
        // 这里不再需要预先读取。

        MH_HOOK(&Sleep, &Hook_Sleep, reinterpret_cast<LPVOID*> (&Real_Sleep));
        MH_HOOK(&SleepEx, &Hook_SleepEx, reinterpret_cast<LPVOID*>(&Real_SleepEx));
        MH_HOOK(&WaitForSingleObject, &Hook_WaitForSingleObject, reinterpret_cast<LPVOID*>(&Real_WaitForSingleObject));
        MH_HOOK(&WaitForSingleObjectEx, &Hook_WaitForSingleObjectEx, reinterpret_cast<LPVOID*>(&Real_WaitForSingleObjectEx));
        MH_HOOK(&WaitForMultipleObjects, &Hook_WaitForMultipleObjects, reinterpret_cast<LPVOID*>(&Real_WaitForMultipleObjects));
        MH_HOOK(&WaitForMultipleObjectsEx, &Hook_WaitForMultipleObjectsEx, reinterpret_cast<LPVOID*>(&Real_WaitForMultipleObjectsEx));
        MH_HOOK(&SetWaitableTimer, &Hook_SetWaitableTimer, reinterpret_cast<LPVOID*>(&Real_SetWaitableTimer));
        MH_HOOK(&SetWaitableTimerEx, &Hook_SetWaitableTimerEx, reinterpret_cast<LPVOID*>(&Real_SetWaitableTimerEx));
        MH_HOOK(&MsgWaitForMultipleObjects, &Hook_MsgWaitForMultipleObjects, reinterpret_cast<LPVOID*>(&Real_MsgWaitForMultipleObjects));
        MH_HOOK(&MsgWaitForMultipleObjectsEx, &Hook_MsgWaitForMultipleObjectsEx, reinterpret_cast<LPVOID*>(&Real_MsgWaitForMultipleObjectsEx));
        MH_HOOK(&SetTimer, &Hook_SetTimer, reinterpret_cast<LPVOID*> (&Real_SetTimer));
        MH_HOOK(&timeGetTime, &Hook_TimeGetTime, reinterpret_cast<LPVOID*> (&Real_TimeGetTime));
        MH_HOOK(&timeSetEvent, &Hook_TimeSetEvent, reinterpret_cast<LPVOID*>(&Real_TimeSetEvent));
        MH_HOOK(&GetMessageTime, &Hook_GetMessageTime, reinterpret_cast<LPVOID*>(&Real_GetMessageTime));
        MH_HOOK(&GetTickCount, &Hook_GetTickCount, reinterpret_cast<LPVOID*> (&Real_GetTickCount));
        MH_HOOK(&GetTickCount64, &Hook_GetTickCount64, reinterpret_cast<LPVOID*> (&Real_GetTickCount64));
        MH_HOOK(&QueryPerformanceCounter, &Hook_QueryPerformanceCounter, reinterpret_cast<LPVOID*> (&Real_QueryPerformanceCounter));
        MH_HOOK(&GetSystemTimeAsFileTime, &Hook_GetSystemTimeAsFileTime, reinterpret_cast<LPVOID*> (&Real_GetSystemTimeAsFileTime));
        MH_HOOK(&GetSystemTimePreciseAsFileTime, &Hook_GetSystemTimePreciseAsFileTime, reinterpret_cast<LPVOID*> (&Real_GetSystemTimePreciseAsFileTime));


        break;
    case DLL_THREAD_ATTACH:
        break;
    case DLL_THREAD_DETACH:
        break;
    case DLL_PROCESS_DETACH:
    {
        {
            std::unique_lock<std::shared_mutex> lock(mutex);
            MH_DisableHook(MH_ALL_HOOKS);
        }
        {
            std::unique_lock<std::shared_mutex> lock(mutex);
            MH_UNHOOK(Real_Sleep);
            MH_UNHOOK(Real_SleepEx);
            MH_UNHOOK(Real_WaitForSingleObject);
            MH_UNHOOK(Real_WaitForSingleObjectEx);
            MH_UNHOOK(Real_WaitForMultipleObjects);
            MH_UNHOOK(Real_WaitForMultipleObjectsEx);
            MH_UNHOOK(Real_SetWaitableTimer);
            MH_UNHOOK(Real_SetWaitableTimerEx);
            MH_UNHOOK(Real_SetTimer);
            MH_UNHOOK(Real_TimeGetTime);
            MH_UNHOOK(Real_TimeSetEvent);
            MH_UNHOOK(Real_GetTickCount);
            MH_UNHOOK(Real_GetTickCount64);
            MH_UNHOOK(Real_QueryPerformanceCounter);
            MH_UNHOOK(Real_GetSystemTimeAsFileTime);
            MH_UNHOOK(Real_GetSystemTimePreciseAsFileTime);
        }
        // Wait for All threads to finish detour api
        Sleep(1000);
        {
            std::unique_lock<std::shared_mutex> lock(mutex);
            if (MH_Uninitialize() != MH_OK)
            {
                // 进程正在退出，同样不能弹模态框；DllMain 在 DETACH 下的返回值无意义，
                // 所以这里也不能 return —— 下面的 SP_Uninstall() 必须照常执行。
                OutputDebugStringA("[speedpatch] MH_Uninitialize failed\n");
            }
        }
        SP_Uninstall();
        break;
    }
    }
    return TRUE;
}
