// Mock native render-tap adapter used to verify the broker/IPC end-to-end without
// injecting a terminal. It intentionally uses only Win32 + the documented binary
// protocol, exercising interoperability independently from Rust's test encoder.

#include <windows.h>

#include <algorithm>
#include <array>
#include <bit>
#include <cstdint>
#include <cstdio>
#include <string>
#include <string_view>
#include <type_traits>
#include <vector>

namespace
{
    constexpr std::array<std::uint8_t, 4> magic{ 'S', 'G', 'N', 'T' };
    constexpr std::uint16_t protocol = 1;
    constexpr std::string_view imageKey = "f11fb145fb56636723b20f30e40aaac672e9de2c9677de363551d82668cbd5cd";
    constexpr std::array<std::uint8_t, 68> imageBytes{
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d,
        0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
        0x08, 0x04, 0x00, 0x00, 0x00, 0xb5, 0x1c, 0x0c, 0x02, 0x00, 0x00, 0x00, 0x0b,
        0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64, 0xf8, 0x0f, 0x00, 0x01,
        0x05, 0x01, 0x01, 0x27, 0x18, 0xe3, 0x66, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    };

    enum class MessageType : std::uint16_t
    {
        hello = 1,
        sourceAdded = 2,
        sourceRemoved = 4,
        frame = 5,
        imageBlob = 6,
        subscribe = 0x101,
        unsubscribe = 0x102,
        requestFull = 0x103,
        shutdown = 0x105,
    };

    template<typename T>
    void append(std::vector<std::uint8_t>& out, const T value)
    {
        static_assert(std::is_integral_v<T>);
        for (std::size_t i = 0; i < sizeof(T); ++i)
        {
            out.push_back(static_cast<std::uint8_t>(static_cast<std::make_unsigned_t<T>>(value) >> (i * 8)));
        }
    }

    void appendString(std::vector<std::uint8_t>& out, const std::string_view value)
    {
        if (value.size() > UINT16_MAX)
        {
            std::terminate();
        }
        append(out, static_cast<std::uint16_t>(value.size()));
        out.insert(out.end(), value.begin(), value.end());
    }

    bool writeAll(const HANDLE pipe, const void* data, std::size_t length)
    {
        const auto* bytes = static_cast<const std::uint8_t*>(data);
        while (length != 0)
        {
            DWORD written = 0;
            const auto chunk = static_cast<DWORD>((std::min)(length, static_cast<std::size_t>(UINT32_MAX)));
            if (!WriteFile(pipe, bytes, chunk, &written, nullptr) || written == 0)
            {
                return false;
            }
            bytes += written;
            length -= written;
        }
        return true;
    }

    bool send(const HANDLE pipe,
              const MessageType type,
              const std::uint64_t nonce,
              const std::uint64_t sequence,
              const std::vector<std::uint8_t>& payload)
    {
        std::vector<std::uint8_t> packet;
        packet.reserve(28 + payload.size());
        packet.insert(packet.end(), magic.begin(), magic.end());
        append(packet, protocol);
        append(packet, static_cast<std::uint16_t>(type));
        append(packet, static_cast<std::uint32_t>(payload.size()));
        append(packet, nonce);
        append(packet, sequence);
        packet.insert(packet.end(), payload.begin(), payload.end());
        return writeAll(pipe, packet.data(), packet.size());
    }

    std::wstring pipeName()
    {
        DWORD session = 0;
        if (!ProcessIdToSessionId(GetCurrentProcessId(), &session))
        {
            return {};
        }
        return L"\\\\.\\pipe\\shellglass-render-tap-" + std::to_wstring(session);
    }

    HANDLE connectBroker()
    {
        const auto name = pipeName();
        while (true)
        {
            const auto pipe = CreateFileW(name.c_str(), GENERIC_READ | GENERIC_WRITE, 0, nullptr, OPEN_EXISTING, 0, nullptr);
            if (pipe != INVALID_HANDLE_VALUE)
            {
                return pipe;
            }
            if (GetLastError() != ERROR_FILE_NOT_FOUND && GetLastError() != ERROR_PIPE_BUSY)
            {
                return INVALID_HANDLE_VALUE;
            }
            WaitNamedPipeW(name.c_str(), 500);
        }
    }

    std::vector<std::uint8_t> hello()
    {
        std::vector<std::uint8_t> out;
        out.push_back(1); // Windows Terminal provider
#if defined(_M_ARM64)
        out.push_back(2);
#else
        out.push_back(1); // x64
#endif
        append(out, GetCurrentProcessId());
        append(out, std::uint32_t{ 0 }); // v1 capabilities are informational
        appendString(out, "mock-v1");
        out.push_back(0); // module hashes
        return out;
    }

    std::vector<std::uint8_t> sourceAdded(const std::uint64_t source, const std::uint64_t hwnd)
    {
        std::vector<std::uint8_t> out;
        append(out, source);
        append(out, std::uint64_t{ 1 }); // generation
        append(out, hwnd);
        append(out, std::uint16_t{ 24 });
        append(out, std::uint16_t{ 80 });
        append(out, std::uint32_t{ 3 }); // focused + visible
        appendString(out, "shellglass native mock");
        return out;
    }

    std::vector<std::uint8_t> frame(const std::uint64_t source, const std::uint64_t frameSequence)
    {
        constexpr std::uint16_t rows = 24;
        constexpr std::uint16_t cols = 80;
        constexpr std::string_view banner = "shellglass native mock adapter (text-frame interoperability check)";
        std::vector<std::uint8_t> out;
        append(out, source);
        append(out, std::uint64_t{ 1 }); // generation
        append(out, frameSequence);
        append(out, rows);
        append(out, cols);
        out.push_back(0); // default fg
        out.push_back(0); // default bg

        append(out, std::uint16_t{ 1 }); // style table
        out.push_back(0); // fg default
        out.push_back(0); // bg default
        append(out, std::uint16_t{ 0 }); // flags
        out.push_back(0); // underline
        out.push_back(0); // underline color
        append(out, UINT32_MAX); // no link
        append(out, std::uint16_t{ 0 }); // link table

        for (std::uint16_t row = 0; row < rows; ++row)
        {
            append(out, cols); // one ordinary cell per column
            for (std::uint16_t col = 0; col < cols; ++col)
            {
                append(out, col);
                out.push_back(1); // occupied columns
                append(out, std::uint16_t{ 0 }); // style id
                const char ch = row == 0 && col < banner.size() ? banner[col] : ' ';
                appendString(out, std::string_view{ &ch, 1 });
            }
        }
        out.push_back(0); // cursor hidden
        out.push_back(0); // default cursor style
        appendString(out, "shellglass native mock");
        append(out, std::uint16_t{ 1 }); // one content-addressed image placement
        append(out, std::int16_t{ 1 });
        append(out, std::uint16_t{ 0 });
        append(out, std::bit_cast<std::uint32_t>(1.0f));
        append(out, std::bit_cast<std::uint32_t>(1.0f));
        out.insert(out.end(), imageKey.begin(), imageKey.end());
        return out;
    }

    std::vector<std::uint8_t> imageBlob(const std::uint64_t source)
    {
        std::vector<std::uint8_t> out;
        append(out, source);
        append(out, std::uint64_t{ 1 });
        out.insert(out.end(), imageKey.begin(), imageKey.end());
        appendString(out, "image/png");
        append(out, static_cast<std::uint32_t>(imageBytes.size()));
        out.insert(out.end(), imageBytes.begin(), imageBytes.end());
        return out;
    }

    bool readExact(const HANDLE pipe, void* data, std::size_t length)
    {
        auto* bytes = static_cast<std::uint8_t*>(data);
        while (length != 0)
        {
            DWORD read = 0;
            if (!ReadFile(pipe, bytes, static_cast<DWORD>(length), &read, nullptr) || read == 0)
            {
                return false;
            }
            bytes += read;
            length -= read;
        }
        return true;
    }

    std::uint16_t le16(const std::uint8_t* p)
    {
        return static_cast<std::uint16_t>(p[0] | (p[1] << 8));
    }

    std::uint32_t le32(const std::uint8_t* p)
    {
        return static_cast<std::uint32_t>(p[0] | (p[1] << 8) | (p[2] << 16) | (p[3] << 24));
    }
}

int wmain()
{
    const auto nonce = (static_cast<std::uint64_t>(GetCurrentProcessId()) << 32) ^ GetTickCount64();
    constexpr std::uint64_t source = 1;
    std::uint64_t frameSequence = 1;
    while (true)
    {
        const auto pipe = connectBroker();
        if (pipe == INVALID_HANDLE_VALUE)
        {
            std::fprintf(stderr, "shellglass-native-mock: broker connection failed (%lu)\n", GetLastError());
            return 1;
        }
        // Sequence is connection-local; process nonce and source generation stay
        // stable so a restarted broker sees the same live adapter re-register.
        std::uint64_t sequence = 1;
        if (!send(pipe, MessageType::hello, nonce, sequence++, hello()) ||
            !send(pipe,
                  MessageType::sourceAdded,
                  nonce,
                  sequence++,
                  sourceAdded(source, reinterpret_cast<std::uint64_t>(GetForegroundWindow()))) ||
            !send(pipe, MessageType::imageBlob, nonce, sequence++, imageBlob(source)))
        {
            CloseHandle(pipe);
            Sleep(250);
            continue;
        }

        bool shutdown = false;
        while (true)
        {
            std::array<std::uint8_t, 28> envelope{};
            if (!readExact(pipe, envelope.data(), envelope.size()) ||
                !std::equal(magic.begin(), magic.end(), envelope.begin()) ||
                le16(envelope.data() + 4) != protocol)
            {
                break;
            }
            const auto type = static_cast<MessageType>(le16(envelope.data() + 6));
            const auto length = le32(envelope.data() + 8);
            if (length > 1024 * 1024)
            {
                break;
            }
            std::vector<std::uint8_t> payload(length);
            if (!readExact(pipe, payload.data(), payload.size()))
            {
                break;
            }
            if (type == MessageType::subscribe || type == MessageType::requestFull)
            {
                if (!send(pipe, MessageType::frame, nonce, sequence++, frame(source, frameSequence++)))
                {
                    break;
                }
            }
            else if (type == MessageType::shutdown)
            {
                shutdown = true;
                break;
            }
        }
        CloseHandle(pipe);
        if (shutdown)
        {
            return 0;
        }
        Sleep(250);
    }
}
