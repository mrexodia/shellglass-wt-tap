// Explicit Win32-console writer used by the real classic-conhost gate and the
// headless-ConPTY compatibility probe.
#include <windows.h>

#include <string>
#include <string_view>

int wmain(const int argc, wchar_t** argv)
{
    Sleep(5000); // leave time for explicit isolated injection
    const auto output = GetStdHandle(STD_OUTPUT_HANDLE);
    SetConsoleOutputCP(CP_UTF8);
    DWORD written = 0;

    if (argc > 1 && _wcsicmp(argv[1], L"CLASSIC") == 0)
    {
        // Shrink the window before the buffer, then establish the tested view.
        SMALL_RECT tiny{ 0, 0, 0, 0 };
        SetConsoleWindowInfo(output, TRUE, &tiny);
        SetConsoleScreenBufferSize(output, { 90, 200 });
        SMALL_RECT window{ 0, 0, 89, 27 };
        SetConsoleWindowInfo(output, TRUE, &window);
        SetConsoleTitleW(L"SHELLGLASS_CLASSIC_TITLE");
        SetConsoleTextAttribute(output, FOREGROUND_RED | FOREGROUND_GREEN | FOREGROUND_INTENSITY | BACKGROUND_BLUE);
        for (int i = 1; i <= 140; ++i)
        {
            const auto line = L"CLASSIC_FRAME_" + std::to_wstring(i) + L"\r\n";
            if (!WriteConsoleW(output, line.data(), static_cast<DWORD>(line.size()), &written, nullptr)) return 1;
            Sleep(40);
        }
        DWORD mode = 0;
        if (GetConsoleMode(output, &mode)) SetConsoleMode(output, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        constexpr std::string_view alternate{ "\x1b[?1049h\x1b[2JSHELLGLASS_CLASSIC_ALT_SCREEN\r\n" };
        if (!WriteConsoleA(output, alternate.data(), static_cast<DWORD>(alternate.size()), &written, nullptr)) return 1;
        Sleep(2000);
        constexpr std::string_view leaveAlternate{ "\x1b[?1049l" };
        if (!WriteConsoleA(output, leaveAlternate.data(), static_cast<DWORD>(leaveAlternate.size()), &written, nullptr)) return 1;
        constexpr std::wstring_view unicode{ L"UNICODE_FIDELITY: \u6f22\u5b57 e\u0301 \U0001F600\r\n" };
        if (!WriteConsoleW(output, unicode.data(), static_cast<DWORD>(unicode.size()), &written, nullptr)) return 1;
        constexpr std::wstring_view marker{ L"SHELLGLASS_CLASSIC_CONHOST_OK\r\n" };
        if (!WriteConsoleW(output, marker.data(), static_cast<DWORD>(marker.size()), &written, nullptr)) return 1;
        Sleep(8000);
        return 0;
    }

    SetConsoleTitleW(L"SHELLGLASS_HEADLESS_TITLE");
    constexpr std::wstring_view first{ L"SHELLGLASS_HEADLESS_CONPTY_OK\r\n" };
    if (!WriteConsoleW(output, first.data(), static_cast<DWORD>(first.size()), &written, nullptr)) return 1;
    Sleep(2000);
    constexpr std::string_view second{ "\x1b[32mSHELLGLASS_HEADLESS_AFTER_ATTACH\x1b[0m\r\n" };
    if (!WriteConsoleA(output, second.data(), static_cast<DWORD>(second.size()), &written, nullptr)) return 1;
    Sleep(8000);
    return 0;
}
