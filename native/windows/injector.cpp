// Explicit same-architecture LoadLibrary injector used to install a verified
// render-tap adapter into a user-selected terminal process. It never discovers
// targets or scans target memory; policy/target selection stays outside it.
#include <windows.h>

#include <array>
#include <cstdint>
#include <cstdio>
#include <filesystem>
#include <string>
#include <vector>

namespace
{
    struct Handle
    {
        HANDLE value = nullptr;
        ~Handle() { if (value) CloseHandle(value); }
        Handle(const Handle&) = delete;
        Handle& operator=(const Handle&) = delete;
        Handle() = default;
    };

    std::vector<std::uint8_t> tokenUser(const HANDLE process)
    {
        Handle token;
        if (!OpenProcessToken(process, TOKEN_QUERY, &token.value)) return {};
        DWORD length = 0;
        GetTokenInformation(token.value, TokenUser, nullptr, 0, &length);
        std::vector<std::uint8_t> buffer(length);
        if (length == 0 || !GetTokenInformation(token.value, TokenUser, buffer.data(), length, &length)) return {};
        return buffer;
    }

    bool sameUser(const HANDLE target)
    {
        const auto current = tokenUser(GetCurrentProcess());
        const auto other = tokenUser(target);
        if (current.empty() || other.empty()) return false;
        const auto* currentUser = reinterpret_cast<const TOKEN_USER*>(current.data());
        const auto* otherUser = reinterpret_cast<const TOKEN_USER*>(other.data());
        return EqualSid(currentUser->User.Sid, otherUser->User.Sid) != FALSE;
    }
}

int wmain(int argc, wchar_t** argv)
{
    if (argc != 3)
    {
        std::fwprintf(stderr, L"usage: shellglass-inject <pid> <absolute-adapter.dll>\n");
        return 2;
    }
    wchar_t* end = nullptr;
    const auto pidValue = std::wcstoul(argv[1], &end, 10);
    if (!end || *end != L'\0' || pidValue == 0 || pidValue > MAXDWORD)
    {
        std::fwprintf(stderr, L"invalid target PID\n");
        return 2;
    }
    std::error_code error;
    const auto dll = std::filesystem::canonical(argv[2], error);
    if (error || !dll.is_absolute())
    {
        std::fwprintf(stderr, L"adapter path is not an existing absolute path\n");
        return 2;
    }
    const auto path = dll.native();
    const auto bytes = (path.size() + 1) * sizeof(wchar_t);

    Handle process;
    process.value = OpenProcess(PROCESS_CREATE_THREAD | PROCESS_QUERY_LIMITED_INFORMATION |
                                    PROCESS_VM_OPERATION | PROCESS_VM_WRITE | PROCESS_VM_READ,
                                FALSE,
                                static_cast<DWORD>(pidValue));
    if (!process.value)
    {
        std::fwprintf(stderr, L"OpenProcess failed (%lu)\n", GetLastError());
        return 1;
    }
    if (!sameUser(process.value))
    {
        std::fwprintf(stderr, L"target does not belong to the current user\n");
        return 1;
    }
    std::array<wchar_t, 32768> image{};
    DWORD imageLength = static_cast<DWORD>(image.size());
    if (!QueryFullProcessImageNameW(process.value, 0, image.data(), &imageLength))
    {
        std::fwprintf(stderr, L"target image query failed\n");
        return 1;
    }
    const auto targetName = std::filesystem::path{ std::wstring_view{ image.data(), imageLength } }.filename();
    const auto adapterName = dll.filename();
    const bool wtPair = _wcsicmp(targetName.c_str(), L"WindowsTerminal.exe") == 0 &&
                        _wcsicmp(adapterName.c_str(), L"shellglass-wt-adapter.dll") == 0;
    const bool conhostPair = _wcsicmp(targetName.c_str(), L"conhost.exe") == 0 &&
                             _wcsicmp(adapterName.c_str(), L"shellglass-conhost-adapter.dll") == 0;
    if (!wtPair && !conhostPair)
    {
        std::fwprintf(stderr, L"target/adapter pair is not an approved terminal render tap\n");
        return 1;
    }
    USHORT processMachine = 0;
    USHORT nativeMachine = 0;
#if defined(_M_ARM64)
    constexpr USHORT injectorMachine = IMAGE_FILE_MACHINE_ARM64;
#else
    constexpr USHORT injectorMachine = IMAGE_FILE_MACHINE_AMD64;
#endif
    if (!IsWow64Process2(process.value, &processMachine, &nativeMachine) ||
        processMachine != IMAGE_FILE_MACHINE_UNKNOWN || nativeMachine != injectorMachine)
    {
        std::fwprintf(stderr, L"target must match this native injector architecture\n");
        return 1;
    }
    void* remote = VirtualAllocEx(process.value, nullptr, bytes, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    if (!remote)
    {
        std::fwprintf(stderr, L"VirtualAllocEx failed (%lu)\n", GetLastError());
        return 1;
    }
    SIZE_T written = 0;
    if (!WriteProcessMemory(process.value, remote, path.c_str(), bytes, &written) || written != bytes)
    {
        VirtualFreeEx(process.value, remote, 0, MEM_RELEASE);
        std::fwprintf(stderr, L"WriteProcessMemory failed (%lu)\n", GetLastError());
        return 1;
    }
    // kernel32 is loaded at the same system-wide address in same-architecture
    // processes; the target architecture check above makes this explicit.
    const auto loadLibrary = GetProcAddress(GetModuleHandleW(L"kernel32.dll"), "LoadLibraryW");
    Handle thread;
    thread.value = CreateRemoteThread(process.value,
                                      nullptr,
                                      0,
                                      reinterpret_cast<LPTHREAD_START_ROUTINE>(loadLibrary),
                                      remote,
                                      0,
                                      nullptr);
    if (!thread.value)
    {
        VirtualFreeEx(process.value, remote, 0, MEM_RELEASE);
        std::fwprintf(stderr, L"CreateRemoteThread failed (%lu)\n", GetLastError());
        return 1;
    }
    const auto wait = WaitForSingleObject(thread.value, 10000);
    DWORD result = 0;
    const bool loaded = wait == WAIT_OBJECT_0 && GetExitCodeThread(thread.value, &result) && result != 0;
    VirtualFreeEx(process.value, remote, 0, MEM_RELEASE);
    if (!loaded)
    {
        std::fwprintf(stderr, L"target LoadLibrary failed or timed out (%lu)\n", GetLastError());
        return 1;
    }
    std::fprintf(stdout, "adapter loaded into PID %lu\n", static_cast<unsigned long>(pidValue));
    return 0;
}
