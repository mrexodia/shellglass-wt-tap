// Deterministic real-WT payload for the isolated render-tap E2E. It uses the
// console's explicit UTF-8 code page plus VT styles so transport-shell encodings
// cannot masquerade as adapter fidelity failures.
#include <windows.h>

#include <algorithm>
#include <array>
#include <cstdio>
#include <cstdlib>
#include <string>
#include <string_view>

namespace
{
    bool writeAll(const HANDLE output, const std::string_view bytes)
    {
        DWORD written = 0;
        return WriteFile(output, bytes.data(), static_cast<DWORD>(bytes.size()), &written, nullptr) &&
               written == bytes.size();
    }

    int resizeCoherence(const HANDLE output)
    {
        if (!writeAll(output, "\x1b[?1049h\x1b[?7l")) return 1;
        const auto deadline = GetTickCount64() + 8'000;
        unsigned int generation = 0;
        while (GetTickCount64() < deadline)
        {
            CONSOLE_SCREEN_BUFFER_INFO info{};
            if (!GetConsoleScreenBufferInfo(output, &info)) return 1;
            const auto cols = static_cast<unsigned int>(info.srWindow.Right - info.srWindow.Left + 1);
            const auto rows = static_cast<unsigned int>(info.srWindow.Bottom - info.srWindow.Top + 1);
            if (cols == 0 || rows == 0 || cols > 1000 || rows > 500) return 1;

            const auto fill = static_cast<char>('A' + generation++ % 26);
            std::string frame{ "\x1b[?2026h\x1b[2J" };
            frame.reserve(static_cast<std::size_t>(rows) * (cols + 16) + 64);
            for (unsigned int row = 0; row < rows; ++row)
            {
                std::array<char, 32> position{};
                const auto positionLength = std::snprintf(position.data(), position.size(), "\x1b[%u;1H", row + 1);
                if (positionLength <= 0) return 1;
                frame.append(position.data(), static_cast<std::size_t>(positionLength));
                frame.append(cols, fill);
            }
            std::array<char, 80> marker{};
            const auto markerLength = std::snprintf(marker.data(), marker.size(),
                                                    "\x1b[1;1HRESIZE_COHERENCE_%c_%ux%u", fill, cols, rows);
            if (markerLength <= 0) return 1;
            frame.append(marker.data(), static_cast<std::size_t>(markerLength));
            frame.append("\x1b[?2026l");
            if (!writeAll(output, frame)) return 1;
            Sleep(30);
        }
        return writeAll(output, "\x1b[?2026l\x1b[?7h\x1b[?1049l") ? 0 : 1;
    }
}

int wmain(const int argc, wchar_t** argv)
{
    const auto output = GetStdHandle(STD_OUTPUT_HANDLE);
    if (output == INVALID_HANDLE_VALUE) return 1;
    DWORD mode = 0;
    if (!GetConsoleMode(output, &mode) || !SetConsoleMode(output, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING)) return 1;
    if (!SetConsoleOutputCP(CP_UTF8)) return 1;
    if (argc > 2 && std::wstring_view{ argv[2] } == L"resize") return resizeCoherence(output);
    if (argc > 1)
    {
        std::array<char, 128> identity{};
        std::size_t length = 0;
        while (argv[1][length] >= 0x20 && argv[1][length] <= 0x7e && length + 3 < identity.size())
        {
            identity[length] = static_cast<char>(argv[1][length]);
            ++length;
        }
        identity[length++] = '\r';
        identity[length++] = '\n';
        DWORD identityWritten = 0;
        const auto identityLength = static_cast<DWORD>(length);
        if (!WriteConsoleA(output, identity.data(), identityLength, &identityWritten, nullptr) || identityWritten != identityLength) return 1;
    }
    constexpr std::wstring_view prelude{
        L"unicode: alpha beta \x6f22\x5b57 \x754c\r\n"
        L"combining: e\x0301  wide: \x754c\r\n"
    };
    DWORD written = 0;
    if (!WriteConsoleW(output, prelude.data(), static_cast<DWORD>(prelude.size()), &written, nullptr) || written != prelude.size()) return 1;
    for (unsigned int i = 0; i < 60; ++i)
    {
        std::array<char, 64> line{};
        const auto length = std::snprintf(line.data(), line.size(), "SCROLL_%02u deterministic viewport line\r\n", i);
        const auto count = length > 0 ? static_cast<DWORD>(length) : 0;
        if (count == 0 || !WriteConsoleA(output, line.data(), count, &written, nullptr) || written != count) return 1;
    }
    for (unsigned int i = 0; i < 300; ++i)
    {
        std::array<char, 64> frame{};
        const auto length = std::snprintf(frame.data(), frame.size(), "\rPERF_FRAME_%03u", i);
        const auto count = length > 0 ? static_cast<DWORD>(length) : 0;
        if (count == 0 || !WriteConsoleA(output, frame.data(), count, &written, nullptr) || written != count) return 1;
        Sleep(16);
    }
    constexpr std::string_view clearPerformance{ "\r\x1b[2K" };
    if (!WriteConsoleA(output, clearPerformance.data(), static_cast<DWORD>(clearPerformance.size()), &written, nullptr) || written != clearPerformance.size()) return 1;
    constexpr std::string_view marker{
        "\x1bPq\"1;1;12;36#1;2;100;0;0#1!12~-!12~-!12~-!12~-!12~-!12~\x1b\\\r\n"
        "\x1b[4:3;58:2::0:255:0mUNDERLINE_COLOR_FIDELITY\x1b[0m\r\n"
        "\x1b]8;;https://example.com/shellglass\x1b\\LINK_FIDELITY\x1b]8;;\x1b\\\r\n"
        "\x1b[5;8mCONCEAL_BLINK_FIDELITY\x1b[0m\r\n"
        "\x1b[5 q\x1b[1;31mSHELLGLASS_REAL_WT_TAP_7F3A\x1b[0m\r\n"
    };
    if (!WriteConsoleA(output, marker.data(), static_cast<DWORD>(marker.size()), &written, nullptr) || written != marker.size()) return 1;
    constexpr std::string_view unicodeMarker{ "UNICODE_FIDELITY: \xe6\xbc\xa2\xe5\xad\x97 \xe7\x95\x8c e\xcc\x81\r\n" };
    if (!WriteFile(output, unicodeMarker.data(), static_cast<DWORD>(unicodeMarker.size()), &written, nullptr) || written != unicodeMarker.size()) return 1;
    if (argc > 1)
    {
        DWORD identityWritten = 0;
        const auto identityLength = static_cast<DWORD>(wcslen(argv[1]));
        if (!WriteConsoleW(output, argv[1], identityLength, &identityWritten, nullptr) || identityWritten != identityLength) return 1;
        constexpr wchar_t newline[]{ L'\r', L'\n' };
        if (!WriteConsoleW(output, newline, 2, &identityWritten, nullptr) || identityWritten != 2) return 1;
    }

    // Optional sustained full-screen output is used by the isolated overload
    // gate. Each frame is wrapped in WT's synchronized-update mode so one
    // application frame reaches the renderer as one coherent presentation.
    // This fixture may allocate; the injected render callback still may not.
    unsigned long stressSeconds = 0;
    bool liveOutput = false;
    bool alternateScreen = false;
    if (argc > 2)
    {
        if (std::wstring_view{ argv[2] } == L"live")
        {
            liveOutput = true;
        }
        else if (std::wstring_view{ argv[2] } == L"alt")
        {
            alternateScreen = true;
        }
        else
        {
            wchar_t* end = nullptr;
            stressSeconds = std::wcstoul(argv[2], &end, 10);
            if (!end || *end != L'\0' || stressSeconds == 0 || stressSeconds > 300) return 2;
        }
    }
    if (alternateScreen)
    {
        constexpr std::string_view mainBefore{ "\r\nWT_MAIN_BEFORE_ALT\r\n" };
        constexpr std::string_view alternate{ "\x1b[?1049h\x1b[2J\x1b[HALT_SCREEN_FIDELITY\r\n" };
        constexpr std::string_view mainAfter{ "\x1b[?1049l\r\nWT_MAIN_AFTER_ALT\r\n" };
        if (!WriteFile(output, mainBefore.data(), static_cast<DWORD>(mainBefore.size()), &written, nullptr) || written != mainBefore.size()) return 1;
        if (!WriteFile(output, alternate.data(), static_cast<DWORD>(alternate.size()), &written, nullptr) || written != alternate.size()) return 1;
        Sleep(2500);
        if (!WriteFile(output, mainAfter.data(), static_cast<DWORD>(mainAfter.size()), &written, nullptr) || written != mainAfter.size()) return 1;
    }
    if (stressSeconds)
    {
        constexpr std::string_view begin{ "\r\nOVERLOAD_BEGIN\r\n" };
        if (!WriteFile(output, begin.data(), static_cast<DWORD>(begin.size()), &written, nullptr) || written != begin.size()) return 1;
        Sleep(1000); // make the begin marker independently observable

        constexpr std::size_t stressRows = 100;
        constexpr std::size_t stressCols = 320;
        std::string frame;
        frame.reserve(stressRows * (stressCols + 2) + 64);
        const auto deadline = GetTickCount64() + static_cast<std::uint64_t>(stressSeconds) * 1000;
        unsigned int number = 0;
        while (GetTickCount64() < deadline)
        {
            frame.assign("\x1b[?1049h\x1b[?2026h\x1b[H");
            for (std::size_t row = 0; row < stressRows; ++row)
            {
                std::array<char, stressCols + 3> line{};
                std::fill_n(line.data(), stressCols, static_cast<char>('A' + (row + number) % 26));
                const auto label = std::snprintf(line.data(), 48, "OVERLOAD_FRAME_%06u_ROW_%03zu ", number, row);
                if (label < 0) return 1;
                // snprintf writes a NUL; replace it so every row remains a
                // true 320-column repaint rather than a short diagnostic line.
                line[static_cast<std::size_t>(label)] = static_cast<char>('A' + (row + number) % 26);
                line[stressCols] = '\r';
                line[stressCols + 1] = '\n';
                frame.append(line.data(), stressCols + 2);
            }
            frame.append("\x1b[?2026l");
            if (!WriteFile(output, frame.data(), static_cast<DWORD>(frame.size()), &written, nullptr) || written != frame.size()) return 1;
            ++number;
            Sleep(8);
        }
        constexpr std::string_view complete{ "\x1b[?2026l\x1b[?1049l\r\nOVERLOAD_COMPLETE\r\n" };
        if (!WriteFile(output, complete.data(), static_cast<DWORD>(complete.size()), &written, nullptr) || written != complete.size()) return 1;
    }
    if (liveOutput)
    {
        // The scrollback gate wheels WT into history while these writes continue.
        // Seeing later LIVE_OUTPUT rows only after returning to the bottom proves
        // the WT tap follows WT's private viewport rather than ConPTY live-bottom.
        for (unsigned int i = 0; i < 300; ++i)
        {
            std::array<char, 64> line{};
            const auto length = std::snprintf(line.data(), line.size(), "LIVE_OUTPUT_%03u still arriving while scrolled\r\n", i);
            const auto count = length > 0 ? static_cast<DWORD>(length) : 0;
            if (!count || !WriteConsoleA(output, line.data(), count, &written, nullptr) || written != count) return 1;
            Sleep(100);
        }
    }
    Sleep(30000);
    return 0;
}
