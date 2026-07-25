// Disposable ConPTY host used by the real conhost render-tap gate. It creates
// a headless conhost, drains its VT output, and deliberately delays the marker
// so the test can inject the adapter before the frame is rendered.
#include <windows.h>

#include <algorithm>
#include <array>
#include <filesystem>
#include <fstream>
#include <string>

namespace
{
    struct Handle
    {
        HANDLE value = nullptr;
        ~Handle() { if (value && value != INVALID_HANDLE_VALUE) CloseHandle(value); }
        Handle() = default;
        Handle(const Handle&) = delete;
        Handle& operator=(const Handle&) = delete;
    };
}

int WINAPI wWinMain(HINSTANCE, HINSTANCE, wchar_t*, int)
{
    Handle inputRead;
    Handle inputWrite;
    Handle outputRead;
    Handle outputWrite;
    if (!CreatePipe(&inputRead.value, &inputWrite.value, nullptr, 0) ||
        !CreatePipe(&outputRead.value, &outputWrite.value, nullptr, 0))
    {
        return 1;
    }

    HPCON pseudoConsole = nullptr;
    if (FAILED(CreatePseudoConsole({ 100, 30 }, inputRead.value, outputWrite.value, 0, &pseudoConsole))) return 1;
    inputRead.value = nullptr;
    outputWrite.value = nullptr;

    SIZE_T attributeBytes = 0;
    InitializeProcThreadAttributeList(nullptr, 1, 0, &attributeBytes);
    auto* attributes = static_cast<PPROC_THREAD_ATTRIBUTE_LIST>(HeapAlloc(GetProcessHeap(), 0, attributeBytes));
    if (!attributes)
    {
        ClosePseudoConsole(pseudoConsole);
        return 1;
    }
    if (!InitializeProcThreadAttributeList(attributes, 1, 0, &attributeBytes) ||
        !UpdateProcThreadAttribute(attributes, 0, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
                                   pseudoConsole, sizeof(pseudoConsole), nullptr, nullptr))
    {
        HeapFree(GetProcessHeap(), 0, attributes);
        ClosePseudoConsole(pseudoConsole);
        return 1;
    }

    STARTUPINFOEXW startup{};
    startup.StartupInfo.cb = sizeof(startup);
    startup.lpAttributeList = attributes;
    PROCESS_INFORMATION process{};
    std::array<wchar_t, 32768> self{};
    const auto selfLength = GetModuleFileNameW(nullptr, self.data(), static_cast<DWORD>(self.size()));
    if (!selfLength || selfLength == self.size())
    {
        ClosePseudoConsole(pseudoConsole);
        return 1;
    }
    const auto child = std::filesystem::path{ std::wstring_view{ self.data(), selfLength } }.parent_path() /
                       L"shellglass-conhost-client-fixture.exe";
    std::wstring command = L"\"" + child.native() + L"\"";
    const auto created = CreateProcessW(child.c_str(), command.data(), nullptr, nullptr, FALSE,
                                        EXTENDED_STARTUPINFO_PRESENT, nullptr, nullptr,
                                        &startup.StartupInfo, &process);
    DeleteProcThreadAttributeList(attributes);
    HeapFree(GetProcessHeap(), 0, attributes);
    if (!created)
    {
        ClosePseudoConsole(pseudoConsole);
        return 1;
    }
    CloseHandle(process.hThread);
    std::array<char, 8192> buffer{};
    std::string captured;
    while (WaitForSingleObject(process.hProcess, 0) == WAIT_TIMEOUT)
    {
        DWORD available = 0;
        if (!PeekNamedPipe(outputRead.value, nullptr, 0, nullptr, &available, nullptr)) break;
        if (!available) { Sleep(10); continue; }
        DWORD read = 0;
        if (!ReadFile(outputRead.value, buffer.data(), (std::min)(available, static_cast<DWORD>(buffer.size())), &read, nullptr)) break;
        captured.append(buffer.data(), read);
    }
    DWORD exitCode = 1;
    GetExitCodeProcess(process.hProcess, &exitCode);
    CloseHandle(process.hProcess);
    ClosePseudoConsole(pseudoConsole);
    std::ofstream{ child.parent_path() / L"conpty-captured.bin", std::ios::binary }
        .write(captured.data(), static_cast<std::streamsize>(captured.size()));
    return static_cast<int>(exitCode);
}
