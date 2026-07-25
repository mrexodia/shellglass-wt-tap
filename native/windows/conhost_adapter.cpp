// Production render-tap adapter for the exact x64 Windows 10 conhost
// 10.0.19045 ABI family. The profile beside this DLL pins the complete module
// hash, RSDS identity, RVAs, and detour prologues. Unknown binaries fail closed.
// Render callbacks only copy into bounded preallocated batches; all allocation,
// UTF conversion, model assembly, and named-pipe I/O run on the worker thread.

#include <windows.h>
#include <bcrypt.h>
#include <tlhelp32.h>

#include <algorithm>
#include <array>
#include <atomic>
#include <cstdint>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <memory>
#include <span>
#include <string>
#include <string_view>
#include <type_traits>
#include <unordered_map>
#include <utility>
#include <vector>

#pragma comment(lib, "bcrypt.lib")

namespace
{
    constexpr std::array<std::uint8_t, 4> wireMagic{ 'S', 'G', 'N', 'T' };
    constexpr std::uint16_t wireVersion = 1;
    constexpr std::size_t maxLines = 1024;
    constexpr std::size_t maxClusters = 131072;
    constexpr std::size_t maxTextUnits = 524288;
    constexpr std::size_t maxTitleUnits = 1024;

#pragma pack(push, 1)
    struct ProfileHeader
    {
        char magic[4];
        std::uint16_t version;
        std::uint16_t machine;
        std::uint32_t imageSize;
        std::uint8_t moduleSha256[32];
        GUID pdbGuid;
        std::uint32_t pdbAge;
        char family[32];
        std::uint32_t entryCount;
    };
    struct ProfileEntry
    {
        std::uint32_t id;
        std::uint32_t rva;
        std::uint8_t expected[16];
    };
#pragma pack(pop)

    struct Coord
    {
        std::int16_t x;
        std::int16_t y;
    };
    struct SmallRect
    {
        std::int16_t left;
        std::int16_t top;
        std::int16_t right;
        std::int16_t bottom;
    };
    template<typename T>
    struct AbiSpan
    {
        T* data;
        std::size_t size;
    };
    struct Cluster
    {
        const wchar_t* text;
        std::size_t length;
        std::size_t columns;
    };
    static_assert(sizeof(Cluster) == 24);
    struct CursorOptions
    {
        Coord position;
        std::uint32_t heightPercent;
        std::uint32_t pixelWidth;
        bool doubleWidth;
        std::uint8_t pad0[3];
        std::uint32_t type;
        bool useColor;
        std::uint8_t pad1[3];
        COLORREF color;
        bool on;
        std::uint8_t pad2[3];
    };
    static_assert(sizeof(CursorOptions) == 32);
    struct Opaque
    {
        std::byte byte;
    };

    // Exact vtable order from the matching conhost public PDB and the terminal
    // source generation used by this system ABI. References to FontInfo are
    // opaque because this adapter never dereferences them.
    class RenderEngineAbi
    {
    public:
        virtual ~RenderEngineAbi() = default;
        virtual HRESULT StartPaint() noexcept = 0;
        virtual HRESULT EndPaint() noexcept = 0;
        virtual HRESULT Present() noexcept = 0;
        virtual HRESULT PrepareForTeardown(bool*) noexcept = 0;
        virtual HRESULT ScrollFrame() noexcept = 0;
        virtual HRESULT Invalidate(const SmallRect*) noexcept = 0;
        virtual HRESULT InvalidateCursor(const Coord*) noexcept = 0;
        virtual HRESULT InvalidateSystem(const RECT*) noexcept = 0;
        virtual HRESULT InvalidateSelection(const std::vector<SmallRect>&) noexcept = 0;
        virtual HRESULT InvalidateScroll(const Coord*) noexcept = 0;
        virtual HRESULT InvalidateAll() noexcept = 0;
        virtual HRESULT InvalidateCircling(bool*) noexcept = 0;
        virtual HRESULT InvalidateTitle(const std::wstring&) noexcept = 0;
        virtual HRESULT PaintBackground() noexcept = 0;
        virtual HRESULT PaintBufferLine(AbiSpan<const Cluster>, Coord, bool) noexcept = 0;
        virtual HRESULT PaintBufferGridLines(std::uint32_t, COLORREF, std::size_t, Coord) noexcept = 0;
        virtual HRESULT PaintSelection(SmallRect) noexcept = 0;
        virtual HRESULT PaintCursor(const CursorOptions&) noexcept = 0;
        virtual HRESULT UpdateDrawingBrushes(COLORREF, COLORREF, std::uint16_t, std::uint8_t, bool) noexcept = 0;
        virtual HRESULT UpdateFont(const Opaque&, Opaque&) noexcept = 0;
        virtual HRESULT UpdateDpi(int) noexcept = 0;
        virtual HRESULT UpdateViewport(SmallRect) noexcept = 0;
        virtual HRESULT GetProposedFont(const Opaque&, Opaque&, int) noexcept = 0;
        virtual SmallRect GetDirtyRectInChars() = 0;
        virtual HRESULT GetFontSize(Coord*) noexcept = 0;
        virtual HRESULT IsGlyphWideByFont(std::wstring_view, bool*) noexcept = 0;
        virtual HRESULT UpdateTitle(const std::wstring&) noexcept = 0;
    };

    struct Style
    {
        COLORREF foreground = RGB(255, 255, 255);
        COLORREF background = RGB(0, 0, 0);
        std::uint16_t flags = 0;
        std::uint8_t underline = 0;
        bool operator==(const Style&) const = default;
    };
    struct ClusterEvent
    {
        std::uint16_t column;
        std::uint8_t width;
        std::uint32_t textOffset;
        std::uint16_t textLength;
        Style style;
    };
    struct LineEvent
    {
        std::uint16_t row;
        std::uint32_t firstCluster;
        std::uint32_t clusterCount;
    };
    struct Batch
    {
        std::array<LineEvent, maxLines> lines{};
        std::array<ClusterEvent, maxClusters> clusters{};
        std::array<wchar_t, maxTextUnits> text{};
        std::array<wchar_t, maxTitleUnits> title{};
        std::uint32_t lineCount = 0;
        std::uint32_t clusterCount = 0;
        std::uint32_t textLength = 0;
        std::uint16_t titleLength = 0;
        std::uint16_t rows = 0;
        std::uint16_t cols = 0;
        COLORREF defaultForeground = RGB(255, 255, 255);
        COLORREF defaultBackground = RGB(0, 0, 0);
        bool hasCursor = false;
        CursorOptions cursor{};
        bool overflow = false;

        void reset(const std::uint16_t newRows, const std::uint16_t newCols) noexcept
        {
            lineCount = clusterCount = textLength = titleLength = 0;
            rows = newRows;
            cols = newCols;
            hasCursor = false;
            overflow = false;
        }
    };

    enum class MessageType : std::uint16_t
    {
        hello = 1,
        sourceAdded = 2,
        sourceUpdated = 3,
        sourceRemoved = 4,
        frame = 5,
        diagnostic = 7,
        subscribe = 0x101,
        unsubscribe = 0x102,
        requestFull = 0x103,
        ping = 0x104,
        shutdown = 0x105,
    };

    template<typename T>
    void append(std::vector<std::uint8_t>& out, const T value)
    {
        static_assert(std::is_integral_v<T>);
        using U = std::make_unsigned_t<T>;
        for (std::size_t i = 0; i < sizeof(T); ++i)
            out.push_back(static_cast<std::uint8_t>(static_cast<U>(value) >> (8 * i)));
    }
    void appendString(std::vector<std::uint8_t>& out, const std::string_view value)
    {
        const auto size = (std::min)(value.size(), static_cast<std::size_t>(UINT16_MAX));
        append(out, static_cast<std::uint16_t>(size));
        out.insert(out.end(), value.begin(), value.begin() + size);
    }
    void appendColor(std::vector<std::uint8_t>& out, const COLORREF value)
    {
        out.push_back(2);
        out.push_back(GetRValue(value));
        out.push_back(GetGValue(value));
        out.push_back(GetBValue(value));
    }
    bool writeAll(const HANDLE pipe, const void* data, std::size_t length)
    {
        auto* bytes = static_cast<const std::uint8_t*>(data);
        while (length)
        {
            DWORD written = 0;
            const auto chunk = static_cast<DWORD>((std::min)(length, static_cast<std::size_t>(UINT32_MAX)));
            if (!WriteFile(pipe, bytes, chunk, &written, nullptr) || !written) return false;
            bytes += written;
            length -= written;
        }
        return true;
    }
    bool sendPacket(const HANDLE pipe, const MessageType type, const std::uint64_t nonce,
                    const std::uint64_t sequence, const std::vector<std::uint8_t>& payload)
    {
        std::vector<std::uint8_t> packet;
        packet.reserve(28 + payload.size());
        packet.insert(packet.end(), wireMagic.begin(), wireMagic.end());
        append(packet, wireVersion);
        append(packet, static_cast<std::uint16_t>(type));
        append(packet, static_cast<std::uint32_t>(payload.size()));
        append(packet, nonce);
        append(packet, sequence);
        packet.insert(packet.end(), payload.begin(), payload.end());
        return writeAll(pipe, packet.data(), packet.size());
    }
    bool sha256(const std::span<const std::uint8_t> bytes, std::uint8_t* output)
    {
        BCRYPT_ALG_HANDLE algorithm = nullptr;
        BCRYPT_HASH_HANDLE hash = nullptr;
        DWORD objectLength = 0;
        DWORD resultLength = 0;
        std::vector<std::uint8_t> object;
        bool ok = BCryptOpenAlgorithmProvider(&algorithm, BCRYPT_SHA256_ALGORITHM, nullptr, 0) >= 0 &&
                  BCryptGetProperty(algorithm, BCRYPT_OBJECT_LENGTH, reinterpret_cast<PUCHAR>(&objectLength),
                                    sizeof(objectLength), &resultLength, 0) >= 0;
        if (ok)
        {
            object.resize(objectLength);
            ok = BCryptCreateHash(algorithm, &hash, object.data(), objectLength, nullptr, 0, 0) >= 0 &&
                 BCryptHashData(hash, const_cast<PUCHAR>(bytes.data()), static_cast<ULONG>(bytes.size()), 0) >= 0 &&
                 BCryptFinishHash(hash, output, 32, 0) >= 0;
        }
        if (hash) BCryptDestroyHash(hash);
        if (algorithm) BCryptCloseAlgorithmProvider(algorithm, 0);
        return ok;
    }

    HMODULE selfModule = nullptr;
    ProfileHeader profile{};
    std::unordered_map<std::uint32_t, ProfileEntry> profileEntries;
    std::array<std::uint8_t, 32> moduleHash{};

    bool loadProfile()
    {
        std::array<wchar_t, 32768> path{};
        const auto length = GetModuleFileNameW(selfModule, path.data(), static_cast<DWORD>(path.size()));
        if (!length || length == path.size()) return false;
        std::filesystem::path profilePath{ std::wstring_view{ path.data(), length } };
        profilePath.replace_extension(L".sgnp");
        std::ifstream input{ profilePath, std::ios::binary };
        std::vector<std::uint8_t> bytes{ std::istreambuf_iterator<char>{ input }, {} };
        if (bytes.size() < sizeof(ProfileHeader) + 32) return false;
        std::array<std::uint8_t, 32> digest{};
        if (!sha256(std::span{ bytes }.first(bytes.size() - digest.size()), digest.data()) ||
            std::memcmp(digest.data(), bytes.data() + bytes.size() - digest.size(), digest.size())) return false;
        std::memcpy(&profile, bytes.data(), sizeof(profile));
        if (std::memcmp(profile.magic, "SGNP", 4) || profile.version != 1 ||
            profile.machine != IMAGE_FILE_MACHINE_AMD64 ||
            std::string_view{ profile.family, strnlen_s(profile.family, sizeof(profile.family)) } != "conhost_10_0_19045" ||
            profile.entryCount != 3 ||
            bytes.size() != sizeof(profile) + profile.entryCount * sizeof(ProfileEntry) + 32) return false;
        const auto* entries = reinterpret_cast<const ProfileEntry*>(bytes.data() + sizeof(profile));
        for (std::uint32_t i = 0; i < profile.entryCount; ++i)
            if (!profileEntries.emplace(entries[i].id, entries[i]).second) return false;
        for (std::uint32_t id = 100; id <= 102; ++id)
            if (!profileEntries.contains(id)) return false;
        return true;
    }
    bool verifyModule(const HMODULE module)
    {
        const auto* base = reinterpret_cast<const std::uint8_t*>(module);
        const auto* dos = reinterpret_cast<const IMAGE_DOS_HEADER*>(base);
        if (dos->e_magic != IMAGE_DOS_SIGNATURE) return false;
        const auto* nt = reinterpret_cast<const IMAGE_NT_HEADERS64*>(base + dos->e_lfanew);
        if (nt->Signature != IMAGE_NT_SIGNATURE || nt->FileHeader.Machine != profile.machine ||
            nt->OptionalHeader.SizeOfImage != profile.imageSize) return false;
        bool pdbMatched = false;
        const auto& directory = nt->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_DEBUG];
        if (directory.VirtualAddress && directory.Size >= sizeof(IMAGE_DEBUG_DIRECTORY))
        {
            const auto* entries = reinterpret_cast<const IMAGE_DEBUG_DIRECTORY*>(base + directory.VirtualAddress);
            for (std::size_t i = 0; i < directory.Size / sizeof(*entries); ++i)
            {
                if (entries[i].Type != IMAGE_DEBUG_TYPE_CODEVIEW || entries[i].SizeOfData < 24) continue;
                const auto* codeView = base + entries[i].AddressOfRawData;
                GUID guid{};
                std::uint32_t age = 0;
                if (!std::memcmp(codeView, "RSDS", 4))
                {
                    std::memcpy(&guid, codeView + 4, sizeof(guid));
                    std::memcpy(&age, codeView + 20, sizeof(age));
                    pdbMatched = !std::memcmp(&guid, &profile.pdbGuid, sizeof(guid)) && age == profile.pdbAge;
                    if (pdbMatched) break;
                }
            }
        }
        if (!pdbMatched) return false;
        std::array<wchar_t, 32768> path{};
        const auto length = GetModuleFileNameW(module, path.data(), static_cast<DWORD>(path.size()));
        if (!length || length == path.size()) return false;
        std::ifstream file{ std::filesystem::path{ std::wstring_view{ path.data(), length } }, std::ios::binary };
        std::vector<std::uint8_t> bytes{ std::istreambuf_iterator<char>{ file }, {} };
        if (bytes.empty() || !sha256(bytes, moduleHash.data()) || moduleHash != std::to_array(profile.moduleSha256)) return false;
        for (const auto& [id, entry] : profileEntries)
        {
            (void)id;
            if (entry.rva > profile.imageSize || sizeof(entry.expected) > profile.imageSize - entry.rva ||
                std::memcmp(base + entry.rva, entry.expected, sizeof(entry.expected))) return false;
        }
        return true;
    }

    void* absoluteTrampoline(void* target, const std::size_t stolen)
    {
        auto* memory = static_cast<std::uint8_t*>(VirtualAlloc(nullptr, stolen + 14, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE));
        if (!memory) return nullptr;
        std::memcpy(memory, target, stolen);
        memory[stolen] = 0xff;
        memory[stolen + 1] = 0x25;
        std::memset(memory + stolen + 2, 0, 4);
        const auto continuation = reinterpret_cast<std::uintptr_t>(target) + stolen;
        std::memcpy(memory + stolen + 6, &continuation, sizeof(continuation));
        DWORD old = 0;
        if (!VirtualProtect(memory, stolen + 14, PAGE_EXECUTE_READ, &old)) return nullptr;
        FlushInstructionCache(GetCurrentProcess(), memory, stolen + 14);
        return memory;
    }
    bool patchHook(void* target, void* hook, const std::size_t stolen)
    {
        if (stolen < 14) return false;
        DWORD old = 0;
        if (!VirtualProtect(target, stolen, PAGE_EXECUTE_READWRITE, &old)) return false;
        auto* bytes = static_cast<std::uint8_t*>(target);
        bytes[0] = 0xff;
        bytes[1] = 0x25;
        std::memset(bytes + 2, 0, 4);
        std::memcpy(bytes + 6, &hook, sizeof(hook));
        std::memset(bytes + 14, 0x90, stolen - 14);
        FlushInstructionCache(GetCurrentProcess(), target, stolen);
        DWORD ignored = 0;
        return VirtualProtect(target, stolen, old, &ignored) != 0;
    }

    DWORD parentProcessId() noexcept
    {
        const auto snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if (snapshot == INVALID_HANDLE_VALUE) return 0;
        PROCESSENTRY32W entry{};
        entry.dwSize = sizeof(entry);
        DWORD parent = 0;
        if (Process32FirstW(snapshot, &entry))
        {
            do
            {
                if (entry.th32ProcessID == GetCurrentProcessId())
                {
                    parent = entry.th32ParentProcessID;
                    break;
                }
            } while (Process32NextW(snapshot, &entry));
        }
        CloseHandle(snapshot);
        return parent;
    }

    HWND processWindow() noexcept
    {
        // For classic consoles Windows attributes ConsoleWindowClass to the
        // console leader (the conhost parent), not conhost.exe itself.
        struct Search { DWORD pid; HWND found; } search{ parentProcessId(), nullptr };
        if (!search.pid) search.pid = GetCurrentProcessId();
        EnumWindows([](HWND hwnd, LPARAM value) -> BOOL {
            auto& state = *reinterpret_cast<Search*>(value);
            DWORD pid = 0;
            GetWindowThreadProcessId(hwnd, &pid);
            if (pid != state.pid) return TRUE;
            std::array<wchar_t, 64> name{};
            if (GetClassNameW(hwnd, name.data(), static_cast<int>(name.size())) > 0 &&
                _wcsicmp(name.data(), L"ConsoleWindowClass") == 0)
            {
                state.found = hwnd;
                return FALSE;
            }
            return TRUE;
        }, reinterpret_cast<LPARAM>(&search));
        return search.found;
    }

    using AddFn = void(__fastcall*)(void*, RenderEngineAbi*);
    using PaintFn = HRESULT(__fastcall*)(void*);
    using TriggerFn = void(__fastcall*)(void*);
    AddFn originalAdd = nullptr;
    PaintFn originalPaint = nullptr;
    TriggerFn triggerRedraw = nullptr;

    class CaptureEngine final : public RenderEngineAbi
    {
    public:
        CaptureEngine()
        {
            source = reinterpret_cast<std::uint64_t>(this);
            QueryPerformanceFrequency(&performanceFrequency);
        }

        HRESULT StartPaint() noexcept override
        {
            activePaint.fetch_add(1);
            if (faulted.load(std::memory_order_acquire) || !subscribed.load() ||
                !dirty.exchange(false, std::memory_order_acq_rel))
            {
                activePaint.fetch_sub(1);
                return S_FALSE;
            }
            if (!current) current = acquireFreeBatch();
            if (!current)
            {
                dirty.store(true, std::memory_order_release);
                activePaint.fetch_sub(1);
                return S_FALSE;
            }
            current->reset(rows.load(std::memory_order_relaxed), cols.load(std::memory_order_relaxed));
            QueryPerformanceCounter(&paintStarted);
            painting = true;
            return S_OK;
        }
        HRESULT EndPaint() noexcept override
        {
            painting = false;
            LARGE_INTEGER ended{};
            QueryPerformanceCounter(&ended);
            const auto micros = static_cast<std::uint64_t>((ended.QuadPart - paintStarted.QuadPart) * 1'000'000 /
                                                           performanceFrequency.QuadPart);
            const auto bucket = micros <= 250 ? 0u : micros <= 500 ? 1u : micros <= 1000 ? 2u : micros <= 2000 ? 3u : 4u;
            performanceBuckets[bucket].fetch_add(1, std::memory_order_relaxed);
            auto maximum = performanceMaxMicros.load(std::memory_order_relaxed);
            while (micros > maximum && !performanceMaxMicros.compare_exchange_weak(maximum, micros, std::memory_order_relaxed)) {}
            performanceCount.fetch_add(1, std::memory_order_release);
            if (!current || current->overflow || !current->rows || !current->cols)
            {
                dirty.store(true, std::memory_order_release);
                activePaint.fetch_sub(1);
                return S_OK;
            }
            const auto oldRows = rows.exchange(current->rows, std::memory_order_acq_rel);
            const auto oldCols = cols.exchange(current->cols, std::memory_order_acq_rel);
            if (oldRows != current->rows || oldCols != current->cols) metadataChanged.store(true, std::memory_order_release);
            // Keep exactly one completed batch and make that slot newest-wins.
            // Under overload, replacing the unconsumed batch bounds both memory
            // and visual latency without ever waiting in the render callback.
            if (auto* replaced = pending.exchange(current, std::memory_order_acq_rel))
            {
                droppedFrames.fetch_add(1, std::memory_order_relaxed);
                current = replaced;
                current->reset(rows.load(std::memory_order_relaxed), cols.load(std::memory_order_relaxed));
            }
            else
            {
                current = acquireFreeBatch();
            }
            activePaint.fetch_sub(1);
            return S_OK;
        }
        HRESULT Present() noexcept override { return S_OK; }
        HRESULT PrepareForTeardown(bool* force) noexcept override { if (force) *force = false; return S_OK; }
        HRESULT ScrollFrame() noexcept override { return S_OK; }
        HRESULT Invalidate(const SmallRect*) noexcept override { markDirty(); return S_OK; }
        HRESULT InvalidateCursor(const Coord*) noexcept override { markDirty(); return S_OK; }
        HRESULT InvalidateSystem(const RECT*) noexcept override { markDirty(); return S_OK; }
        HRESULT InvalidateSelection(const std::vector<SmallRect>&) noexcept override { markDirty(); return S_OK; }
        HRESULT InvalidateScroll(const Coord*) noexcept override { markDirty(); return S_OK; }
        HRESULT InvalidateAll() noexcept override { markDirty(); return S_OK; }
        HRESULT InvalidateCircling(bool* force) noexcept override { if (force) *force = false; markDirty(); return S_OK; }
        HRESULT InvalidateTitle(const std::wstring& title) noexcept override { copyTitle(title); markDirty(); return S_OK; }
        HRESULT PaintBackground() noexcept override { return S_OK; }
        HRESULT PaintBufferLine(const AbiSpan<const Cluster> span, const Coord point, bool) noexcept override
        {
#if defined(SHELLGLASS_CALLBACK_FAULT_TEST)
            if (!faulted.load(std::memory_order_acquire))
            {
                disableCapture();
                return S_OK;
            }
#endif
            if (!painting || !current || point.y < 0 || point.y >= 500 || point.x < 0 || span.size > maxClusters) return S_OK;
            if (current->lineCount >= maxLines || span.size > maxClusters - current->clusterCount)
            {
                current->overflow = true;
                return S_OK;
            }
            auto& line = current->lines[current->lineCount++];
            line.row = static_cast<std::uint16_t>(point.y);
            line.firstCluster = current->clusterCount;
            line.clusterCount = 0;
            auto column = static_cast<std::int32_t>(point.x);
            for (std::size_t i = 0; i < span.size; ++i)
            {
                const auto& input = span.data[i];
                if (input.columns < 1 || input.columns > 2 || column >= 1000) continue;
                if (input.length > UINT16_MAX || input.length > maxTextUnits - current->textLength)
                {
                    current->overflow = true;
                    break;
                }
                auto& output = current->clusters[current->clusterCount++];
                output.column = static_cast<std::uint16_t>(column);
                output.width = static_cast<std::uint8_t>(input.columns);
                output.textOffset = current->textLength;
                output.textLength = static_cast<std::uint16_t>(input.length);
                output.style = style;
                if (input.length)
                {
                    std::memcpy(current->text.data() + current->textLength, input.text, input.length * sizeof(wchar_t));
                    current->textLength += static_cast<std::uint32_t>(input.length);
                }
                ++line.clusterCount;
                column += static_cast<std::int32_t>(input.columns);
            }
            current->rows = (std::max)(current->rows, static_cast<std::uint16_t>(point.y + 1));
            current->cols = (std::max)(current->cols, static_cast<std::uint16_t>((std::clamp)(column, 1, 1000)));
            return S_OK;
        }
        HRESULT PaintBufferGridLines(std::uint32_t, COLORREF, std::size_t, Coord) noexcept override { return S_OK; }
        HRESULT PaintSelection(SmallRect) noexcept override { return S_OK; }
        HRESULT PaintCursor(const CursorOptions& options) noexcept override
        {
            if (painting && current) { current->hasCursor = true; current->cursor = options; }
            return S_OK;
        }
        HRESULT UpdateDrawingBrushes(const COLORREF foreground, const COLORREF background, const std::uint16_t legacy,
                                     const std::uint8_t extended, const bool defaults) noexcept override
        {
            style = {};
            style.foreground = foreground;
            style.background = background;
            if (extended & 0x01) style.flags |= 0x01;
            if (extended & 0x80) style.flags |= 0x02;
            if (extended & 0x02) style.flags |= 0x04;
            if (extended & 0x10) style.flags |= 0x08;
            if (extended & 0x08) style.flags |= 0x10;
            if (extended & 0x04) style.flags |= 0x20;
            if (legacy & 0x4000) style.flags |= 0x40;
            if ((extended & 0x20) || (legacy & 0x8000)) style.underline = 1;
            if (extended & 0x40) style.underline = 2;
            if (defaults && painting && current)
            {
                current->defaultForeground = foreground;
                current->defaultBackground = background;
            }
            return S_OK;
        }
        HRESULT UpdateFont(const Opaque&, Opaque&) noexcept override { return S_OK; }
        HRESULT UpdateDpi(int) noexcept override { return S_OK; }
        HRESULT UpdateViewport(const SmallRect viewport) noexcept override
        {
            const auto width = static_cast<int>(viewport.right) - viewport.left + 1;
            const auto height = static_cast<int>(viewport.bottom) - viewport.top + 1;
            if (width > 0 && width <= 1000 && height > 0 && height <= 500)
            {
                cols.store(static_cast<std::uint16_t>(width), std::memory_order_release);
                rows.store(static_cast<std::uint16_t>(height), std::memory_order_release);
                metadataChanged.store(true, std::memory_order_release);
                markDirty();
            }
            return S_OK;
        }
        HRESULT GetProposedFont(const Opaque&, Opaque&, int) noexcept override { return E_NOTIMPL; }
        SmallRect GetDirtyRectInChars() override
        {
            // AddRenderEngine does not replay UpdateViewport. Request the bounded
            // maximum on every paint; Renderer intersects it with its authoritative
            // viewport, and PaintBufferLine then infers the exact dimensions.
            return { 0, 0, 999, 499 };
        }
        HRESULT GetFontSize(Coord* size) noexcept override { if (size) *size = { 1, 1 }; return S_OK; }
        HRESULT IsGlyphWideByFont(std::wstring_view, bool* result) noexcept override { if (result) *result = false; return S_OK; }
        HRESULT UpdateTitle(const std::wstring& title) noexcept override { copyTitle(title); return S_OK; }

        void markDirty() noexcept { dirty.store(true, std::memory_order_release); }
        void attach(void* value) noexcept
        {
            void* expected = nullptr;
            if (renderer.compare_exchange_strong(expected, value, std::memory_order_acq_rel))
            {
                attached.store(true, std::memory_order_release);
            }
        }
        __declspec(guard(nocf)) void requestFull() noexcept
        {
            if (faulted.load(std::memory_order_acquire)) return;
            if (!current)
            {
                try
                {
                    for (auto& item : batches) item = std::make_unique<Batch>();
                }
                catch (...) { disableCapture(); return; }
                current = batches[0].get();
                free.store(batches[1].get(), std::memory_order_release);
                spare.store(batches[2].get(), std::memory_order_release);
            }
            releaseRequested.store(false, std::memory_order_release);
            subscribed.store(true);
            markDirty();
            if (const auto value = renderer.load(std::memory_order_acquire)) triggerRedraw(value);
        }
        void disableCapture() noexcept
        {
            faulted.store(true, std::memory_order_release);
            park();
            dirty.store(false, std::memory_order_release);
            painting = false;
            if (current) current->overflow = true;
        }
        void park() noexcept
        {
            subscribed.store(false);
            releaseRequested.store(true, std::memory_order_release);
        }
        void reclaimIfDormant() noexcept
        {
            if (!releaseRequested.load(std::memory_order_acquire) || subscribed.load() || activePaint.load() != 0) return;
            current = nullptr;
            pending.store(nullptr, std::memory_order_release);
            free.store(nullptr, std::memory_order_release);
            spare.store(nullptr, std::memory_order_release);
            for (auto& batch : batches) batch.reset();
            matrix.clear();
            matrix.shrink_to_fit();
            modelRows = 0;
            modelCols = 0;
            releaseRequested.store(false, std::memory_order_release);
        }
        Batch* takeBatch() noexcept { return pending.exchange(nullptr, std::memory_order_acq_rel); }
        void releaseBatch(Batch* batch) noexcept
        {
            Batch* expected = nullptr;
            if (free.compare_exchange_strong(expected, batch, std::memory_order_release, std::memory_order_relaxed)) return;
            expected = nullptr;
            if (!spare.compare_exchange_strong(expected, batch, std::memory_order_release, std::memory_order_relaxed))
            {
                // With three batches, both return slots cannot be occupied while
                // this worker owns another unless ownership was corrupted.
                disableCapture();
            }
        }
        void copyTitle(const std::wstring_view title) noexcept
        {
            if (!painting || !current) return;
            current->titleLength = static_cast<std::uint16_t>((std::min)(title.size(), maxTitleUnits));
            if (current->titleLength)
                std::memcpy(current->title.data(), title.data(), current->titleLength * sizeof(wchar_t));
        }

        std::uint64_t source = 0;
        std::atomic<void*> renderer{ nullptr };
        std::atomic<bool> attached{ false };
        std::atomic<bool> faulted{ false };
        std::atomic<bool> subscribed{ false };
        std::atomic<bool> releaseRequested{ false };
        std::atomic<std::uint32_t> activePaint{ 0 };
        std::atomic<bool> dirty{ true };
        std::atomic<bool> metadataChanged{ true };
        std::atomic<std::uint16_t> rows{ 1 };
        std::atomic<std::uint16_t> cols{ 1 };
        std::atomic<std::uint64_t> owner{ 0 };
        std::atomic<std::uint64_t> performanceCount{ 0 };
        std::array<std::atomic<std::uint64_t>, 5> performanceBuckets{};
        std::atomic<std::uint64_t> performanceMaxMicros{ 0 };
        std::atomic<std::uint64_t> droppedFrames{ 0 };
        bool performanceSent = false;
        bool faultDiagnosticSent = false;
        bool sourceSent = false;
        std::uint64_t frameSequence = 1;
        struct Cell
        {
            std::string text{ " " };
            std::uint8_t width = 1;
            bool continuation = false;
            Style style{};
        };
        std::vector<Cell> matrix;
        std::uint16_t modelRows = 0;
        std::uint16_t modelCols = 0;
        std::string modelTitle;
        COLORREF modelDefaultForeground = RGB(255, 255, 255);
        COLORREF modelDefaultBackground = RGB(0, 0, 0);
        bool cursorVisible = false;
        std::uint16_t cursorRow = 0;
        std::uint16_t cursorCol = 0;
        std::uint8_t cursorStyle = 0;

    private:
        LARGE_INTEGER performanceFrequency{};
        LARGE_INTEGER paintStarted{};
        Batch* acquireFreeBatch() noexcept
        {
            if (auto* batch = free.exchange(nullptr, std::memory_order_acq_rel)) return batch;
            return spare.exchange(nullptr, std::memory_order_acq_rel);
        }

        std::array<std::unique_ptr<Batch>, 3> batches;
        Batch* current = nullptr;
        std::atomic<Batch*> spare{ nullptr };
        std::atomic<Batch*> pending{ nullptr };
        std::atomic<Batch*> free{ nullptr };
        bool painting = false;
        Style style{};
    };

    std::unique_ptr<CaptureEngine> engine;

    __declspec(guard(nocf)) void __fastcall hookedAdd(void* renderer, RenderEngineAbi* added)
    {
        if (engine && !engine->attached.load(std::memory_order_acquire))
        {
            engine->attach(renderer);
            originalAdd(renderer, engine.get());
        }
        originalAdd(renderer, added);
    }
    __declspec(guard(nocf)) HRESULT __fastcall hookedPaint(void* renderer)
    {
        if (engine && !engine->attached.load(std::memory_order_acquire))
        {
            engine->attach(renderer);
            originalAdd(renderer, engine.get());
        }
        return originalPaint(renderer);
    }

    void* addTrampoline(void* target)
    {
        // Pinned prologue: mov [rsp+10],rdx; sub rsp,28; test rdx,rdx;
        // je rel32. Preserve both paths with absolute jumps so trampoline
        // placement is independent of the module's +/-2 GiB neighborhood.
        constexpr std::size_t size = 46;
        auto* memory = static_cast<std::uint8_t*>(VirtualAlloc(nullptr, size, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE));
        if (!memory) return nullptr;
        const auto* source = static_cast<const std::uint8_t*>(target);
        std::memcpy(memory, source, 12);
        memory[12] = 0x0f; memory[13] = 0x85; // jne to continuation stub
        const std::int32_t skip = 14;
        std::memcpy(memory + 14, &skip, sizeof(skip));
        memory[18] = 0xff; memory[19] = 0x25; std::memset(memory + 20, 0, 4);
        std::int32_t displacement = 0;
        std::memcpy(&displacement, source + 14, sizeof(displacement));
        const auto nullTarget = reinterpret_cast<std::uintptr_t>(source + 18 + displacement);
        std::memcpy(memory + 24, &nullTarget, sizeof(nullTarget));
        memory[32] = 0xff; memory[33] = 0x25; std::memset(memory + 34, 0, 4);
        const auto continuation = reinterpret_cast<std::uintptr_t>(source + 18);
        std::memcpy(memory + 38, &continuation, sizeof(continuation));
        DWORD old = 0;
        if (!VirtualProtect(memory, size, PAGE_EXECUTE_READ, &old)) return nullptr;
        FlushInstructionCache(GetCurrentProcess(), memory, size);
        return memory;
    }

    bool installHook(const HMODULE module)
    {
        auto* base = reinterpret_cast<std::uint8_t*>(module);
        auto* add = base + profileEntries.at(100).rva;
        triggerRedraw = reinterpret_cast<TriggerFn>(base + profileEntries.at(101).rva);
        auto* paint = base + profileEntries.at(102).rva;
        originalAdd = reinterpret_cast<AddFn>(addTrampoline(add));
        originalPaint = reinterpret_cast<PaintFn>(absoluteTrampoline(paint, 15));
        return originalAdd && originalPaint &&
               patchHook(add, reinterpret_cast<void*>(hookedAdd), 18) &&
               patchHook(paint, reinterpret_cast<void*>(hookedPaint), 15);
    }

    std::string utf8(const wchar_t* text, const std::size_t length)
    {
        if (!length) return {};
        const auto count = WideCharToMultiByte(CP_UTF8, WC_ERR_INVALID_CHARS, text, static_cast<int>(length),
                                               nullptr, 0, nullptr, nullptr);
        if (count <= 0) return "\xef\xbf\xbd";
        std::string out(static_cast<std::size_t>(count), '\0');
        WideCharToMultiByte(CP_UTF8, WC_ERR_INVALID_CHARS, text, static_cast<int>(length), out.data(), count, nullptr, nullptr);
        return out;
    }
    void applyBatch(CaptureEngine& target, const Batch& batch)
    {
        if (!batch.rows || !batch.cols) return;
        if (target.modelRows != batch.rows || target.modelCols != batch.cols)
        {
            target.modelRows = batch.rows;
            target.modelCols = batch.cols;
            target.matrix.assign(static_cast<std::size_t>(batch.rows) * batch.cols, {});
        }
        else
        {
            std::fill(target.matrix.begin(), target.matrix.end(), CaptureEngine::Cell{});
        }
        for (std::uint32_t i = 0; i < batch.lineCount; ++i)
        {
            const auto& line = batch.lines[i];
            if (line.row >= target.modelRows) continue;
            for (std::uint32_t j = 0; j < line.clusterCount; ++j)
            {
                const auto& event = batch.clusters[line.firstCluster + j];
                if (event.column >= target.modelCols) continue;
                auto& cell = target.matrix[static_cast<std::size_t>(line.row) * target.modelCols + event.column];
                cell.text = utf8(batch.text.data() + event.textOffset, event.textLength);
                cell.width = event.width;
                cell.continuation = false;
                cell.style = event.style;
                if (event.width == 2 && event.column + 1 < target.modelCols)
                {
                    auto& continuation = target.matrix[static_cast<std::size_t>(line.row) * target.modelCols + event.column + 1];
                    continuation = {};
                    continuation.continuation = true;
                }
            }
        }
        if (batch.titleLength) target.modelTitle = utf8(batch.title.data(), batch.titleLength);
        target.modelDefaultForeground = batch.defaultForeground;
        target.modelDefaultBackground = batch.defaultBackground;
        target.cursorVisible = batch.hasCursor && batch.cursor.on && batch.cursor.position.x >= 0 && batch.cursor.position.y >= 0 &&
                               batch.cursor.position.x < target.modelCols && batch.cursor.position.y < target.modelRows;
        if (target.cursorVisible)
        {
            target.cursorRow = static_cast<std::uint16_t>(batch.cursor.position.y);
            target.cursorCol = static_cast<std::uint16_t>(batch.cursor.position.x);
            target.cursorStyle = batch.cursor.type == 1 ? 6 : batch.cursor.type == 2 ? 4 : 2;
        }
    }
    std::vector<std::uint8_t> framePayload(CaptureEngine& target)
    {
        std::vector<Style> styles;
        auto styleIndex = [&](const Style& style) {
            const auto found = std::find(styles.begin(), styles.end(), style);
            if (found != styles.end()) return static_cast<std::uint16_t>(found - styles.begin());
            if (styles.size() >= 4096) return std::uint16_t{ 0 };
            styles.push_back(style);
            return static_cast<std::uint16_t>(styles.size() - 1);
        };
        for (const auto& cell : target.matrix) if (!cell.continuation) styleIndex(cell.style);
        if (styles.empty()) styles.push_back({});
        std::vector<std::uint8_t> out;
        out.reserve(target.matrix.size() * 12);
        append(out, target.source);
        append(out, std::uint64_t{ 1 });
        append(out, target.frameSequence++);
        append(out, target.modelRows);
        append(out, target.modelCols);
        appendColor(out, target.modelDefaultForeground);
        appendColor(out, target.modelDefaultBackground);
        append(out, static_cast<std::uint16_t>(styles.size()));
        for (const auto& style : styles)
        {
            appendColor(out, style.foreground);
            appendColor(out, style.background);
            append(out, style.flags);
            out.push_back(style.underline);
            appendColor(out, style.foreground);
            append(out, UINT32_MAX);
        }
        append(out, std::uint16_t{ 0 });
        for (std::uint16_t row = 0; row < target.modelRows; ++row)
        {
            std::uint16_t count = 0;
            for (std::uint16_t col = 0; col < target.modelCols; ++col)
                if (!target.matrix[static_cast<std::size_t>(row) * target.modelCols + col].continuation) ++count;
            append(out, count);
            for (std::uint16_t col = 0; col < target.modelCols; ++col)
            {
                const auto& cell = target.matrix[static_cast<std::size_t>(row) * target.modelCols + col];
                if (cell.continuation) continue;
                append(out, col);
                out.push_back(cell.width);
                append(out, styleIndex(cell.style));
                appendString(out, cell.text);
            }
        }
        out.push_back(target.cursorVisible ? 1 : 0);
        if (target.cursorVisible) { append(out, target.cursorRow); append(out, target.cursorCol); }
        out.push_back(target.cursorStyle);
        appendString(out, target.modelTitle);
        append(out, std::uint16_t{ 0 });
        return out;
    }
    std::vector<std::uint8_t> helloPayload()
    {
        std::vector<std::uint8_t> out;
        out.push_back(2);
        out.push_back(1);
        append(out, GetCurrentProcessId());
        append(out, std::uint32_t{ 0 });
        appendString(out, "conhost_10_0_19045");
        out.push_back(1);
        out.insert(out.end(), moduleHash.begin(), moduleHash.end());
        return out;
    }
    std::vector<std::uint8_t> sourcePayload(const CaptureEngine& target)
    {
        std::vector<std::uint8_t> out;
        append(out, target.source);
        append(out, std::uint64_t{ 1 });
        append(out, target.owner.load());
        append(out, target.rows.load());
        append(out, target.cols.load());
        // Headless ConPTY hosts intentionally have no top-level HWND. They stay
        // registered, but the broker will not select one that was never focused.
        std::uint32_t flags = 2;
        const auto hwnd = reinterpret_cast<HWND>(target.owner.load());
        if (hwnd && GetForegroundWindow() == hwnd) flags |= 1;
        append(out, flags);
        appendString(out, target.modelTitle.empty() ? "Console" : target.modelTitle);
        return out;
    }
    std::vector<std::uint8_t> updatedPayload(const CaptureEngine& target)
    {
        std::vector<std::uint8_t> out;
        append(out, target.source);
        append(out, std::uint64_t{ 1 });
        out.push_back(0x1f);
        append(out, target.owner.load());
        append(out, target.rows.load());
        append(out, target.cols.load());
        const auto hwnd = reinterpret_cast<HWND>(target.owner.load());
        out.push_back(hwnd && GetForegroundWindow() == hwnd ? 1 : 0);
        out.push_back(1);
        appendString(out, target.modelTitle.empty() ? "Console" : target.modelTitle);
        return out;
    }
    std::vector<std::uint8_t> performancePayload(const CaptureEngine& target)
    {
        const auto count = target.performanceCount.load(std::memory_order_relaxed);
        const auto threshold = (count * 95 + 99) / 100;
        std::uint64_t cumulative = 0;
        constexpr std::array<std::uint64_t, 5> ceilings{ 250, 500, 1000, 2000, 999999 };
        std::uint64_t p95 = ceilings.back();
        for (std::size_t i = 0; i < ceilings.size(); ++i)
        {
            cumulative += target.performanceBuckets[i].load(std::memory_order_relaxed);
            if (cumulative >= threshold) { p95 = ceilings[i]; break; }
        }
        const auto text = std::string{ "conhost render callback p95<=" } + std::to_string(p95) +
                          "us max=" + std::to_string(target.performanceMaxMicros.load(std::memory_order_relaxed)) +
                          "us count=" + std::to_string(count) +
                          " dropped=" + std::to_string(target.droppedFrames.load(std::memory_order_relaxed));
        std::vector<std::uint8_t> out;
        out.push_back(1);
        append(out, target.source);
        append(out, std::uint16_t{ 202 });
        appendString(out, text);
        return out;
    }

    std::vector<std::uint8_t> removedPayload(const CaptureEngine& target)
    {
        std::vector<std::uint8_t> out;
        append(out, target.source);
        append(out, std::uint64_t{ 1 });
        return out;
    }
    std::vector<std::uint8_t> faultPayload(const CaptureEngine& target)
    {
        std::vector<std::uint8_t> out;
        out.push_back(1);
        append(out, target.source);
        append(out, std::uint16_t{ 212 });
        appendString(out, "conhost capture provider disabled after an internal callback fault");
        return out;
    }

    std::wstring pipeName()
    {
        DWORD session = 0;
        if (!ProcessIdToSessionId(GetCurrentProcessId(), &session)) return {};
        return L"\\\\.\\pipe\\shellglass-render-tap-" + std::to_wstring(session);
    }
    HANDLE connectBroker()
    {
        const auto name = pipeName();
        return CreateFileW(name.c_str(), GENERIC_READ | GENERIC_WRITE, 0, nullptr, OPEN_EXISTING, 0, nullptr);
    }
    std::uint16_t le16(const std::uint8_t* p) { return static_cast<std::uint16_t>(p[0] | (p[1] << 8)); }
    std::uint32_t le32(const std::uint8_t* p) { return static_cast<std::uint32_t>(p[0] | (p[1] << 8) | (p[2] << 16) | (p[3] << 24)); }
    std::uint64_t le64(const std::uint8_t* p)
    {
        std::uint64_t value = 0;
        for (std::size_t i = 0; i < 8; ++i) value |= static_cast<std::uint64_t>(p[i]) << (8 * i);
        return value;
    }
    bool readExact(const HANDLE pipe, void* output, std::size_t length)
    {
        auto* bytes = static_cast<std::uint8_t*>(output);
        while (length)
        {
            DWORD count = 0;
            if (!ReadFile(pipe, bytes, static_cast<DWORD>(length), &count, nullptr) || !count) return false;
            bytes += count;
            length -= count;
        }
        return true;
    }

    DWORD WINAPI worker(void*)
    {
        // This ABI generation's headless ConPTY fast path bypasses renderer
        // fan-out for application text. Publishing its blank repaint would be
        // worse than no source, so remain fail closed until a verified headless
        // family has an authoritative capture boundary.
        if (std::wstring_view{ GetCommandLineW() }.find(L"--headless") != std::wstring_view::npos) return 1;
        const auto module = GetModuleHandleW(nullptr);
        engine = std::make_unique<CaptureEngine>();
        if (!module || !loadProfile() || !verifyModule(module) || !installHook(module)) return 1;
        const auto nonce = (static_cast<std::uint64_t>(GetCurrentProcessId()) << 32) ^ GetTickCount64();
        while (true)
        {
            const auto pipe = connectBroker();
            if (pipe == INVALID_HANDLE_VALUE)
            {
                engine->reclaimIfDormant();
                Sleep(250);
                continue;
            }
            std::uint64_t sequence = 1;
            if (!sendPacket(pipe, MessageType::hello, nonce, sequence++, helloPayload()))
            {
                CloseHandle(pipe);
                engine->park();
                engine->reclaimIfDormant();
                continue;
            }
            engine->sourceSent = false;
            std::uint64_t nextOwnerCheck = 0;
            bool connected = true;
            while (connected)
            {
                engine->reclaimIfDormant();
                if (engine->attached.load(std::memory_order_acquire))
                {
                    if (engine->faulted.load(std::memory_order_acquire))
                    {
                        if (engine->sourceSent)
                        {
                            connected = sendPacket(pipe, MessageType::sourceRemoved, nonce, sequence++, removedPayload(*engine));
                            engine->sourceSent = false;
                            if (!connected) break;
                        }
                        if (!engine->faultDiagnosticSent)
                        {
                            connected = sendPacket(pipe, MessageType::diagnostic, nonce, sequence++, faultPayload(*engine));
                            engine->faultDiagnosticSent = connected;
                        }
                        if (!connected) break;
                    }
                    else
                    {
                        const auto now = GetTickCount64();
                        if (now >= nextOwnerCheck)
                        {
                            nextOwnerCheck = now + 1000;
                            const auto currentOwner = reinterpret_cast<std::uint64_t>(processWindow());
                            if (currentOwner != engine->owner.exchange(currentOwner)) engine->metadataChanged.store(true);
                        }
                        if (!engine->sourceSent)
                        {
                            connected = sendPacket(pipe, MessageType::sourceAdded, nonce, sequence++, sourcePayload(*engine));
                            engine->sourceSent = connected;
                        }
                        else if (engine->metadataChanged.exchange(false))
                            connected = sendPacket(pipe, MessageType::sourceUpdated, nonce, sequence++, updatedPayload(*engine));
                        if (auto* batch = engine->takeBatch())
                        {
                            applyBatch(*engine, *batch);
                            if (engine->sourceSent)
                                connected = sendPacket(pipe, MessageType::frame, nonce, sequence++, framePayload(*engine));
                            engine->releaseBatch(batch);
                        }
                        if (!engine->performanceSent && engine->performanceCount.load() >= 120)
                        {
                            connected = sendPacket(pipe, MessageType::diagnostic, nonce, sequence++, performancePayload(*engine));
                            engine->performanceSent = connected;
                        }
                    }
                }
                if (!connected) break;
                DWORD available = 0;
                std::array<std::uint8_t, 28> envelope{};
                DWORD peeked = 0;
                if (!PeekNamedPipe(pipe, envelope.data(), static_cast<DWORD>(envelope.size()), &peeked, &available, nullptr)) break;
                if (available >= envelope.size() && peeked == envelope.size())
                {
                    const auto length = le32(envelope.data() + 8);
                    if (length > 1024 * 1024) break;
                    if (available >= envelope.size() + length)
                    {
                        if (!readExact(pipe, envelope.data(), envelope.size()) ||
                            !std::equal(wireMagic.begin(), wireMagic.end(), envelope.begin()) ||
                            le16(envelope.data() + 4) != wireVersion) break;
                        std::vector<std::uint8_t> payload(length);
                        if (!readExact(pipe, payload.data(), payload.size())) break;
                        const auto type = static_cast<MessageType>(le16(envelope.data() + 6));
                        if ((type == MessageType::subscribe || type == MessageType::unsubscribe ||
                             type == MessageType::requestFull) && payload.size() >= 16 &&
                            le64(payload.data()) == engine->source && le64(payload.data() + 8) == 1)
                        {
                            if (type == MessageType::unsubscribe) engine->park();
                            else engine->requestFull();
                        }
                        else if (type == MessageType::shutdown)
                        {
                            CloseHandle(pipe);
                            engine->park();
                            engine->reclaimIfDormant();
                            return 0;
                        }
                    }
                }
                Sleep(2);
            }
            CloseHandle(pipe);
            engine->park();
            engine->reclaimIfDormant();
            engine->sourceSent = false;
            Sleep(250);
        }
    }
}

BOOL WINAPI DllMain(const HINSTANCE instance, const DWORD reason, void*)
{
    if (reason == DLL_PROCESS_ATTACH)
    {
        selfModule = instance;
        DisableThreadLibraryCalls(instance);
        if (const auto thread = CreateThread(nullptr, 0, worker, nullptr, 0, nullptr)) CloseHandle(thread);
    }
    return TRUE;
}
