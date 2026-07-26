// Production Windows Terminal render-tap adapter for the verified wt_1_24 ABI
// families (stock 1.24.11911.0 and 1.24.11321.0, x64). It is loaded into
// WindowsTerminal.exe by an external injector. Every
// executable address comes from a hash/PDB/prologue-verified .sgnp profile; no
// signature scan or guessed target layout is used. Render callbacks only copy
// into fixed-capacity batches and perform lock-free pointer exchanges. Named-pipe
// I/O, UTF conversion, model assembly, and frame encoding run on a worker.

#include <windows.h>
#include <bcrypt.h>
#include <wincodec.h>

#include <algorithm>
#include <array>
#include <bit>
#include <atomic>
#include <cstdint>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <iterator>
#include <memory>
#include <mutex>
#include <span>
#include <string>
#include <string_view>
#include <type_traits>
#include <unordered_map>
#include <unordered_set>
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
    constexpr std::size_t maxSelections = 256;
    constexpr std::size_t maxCaptureStyles = 4096;
    constexpr std::size_t maxImageSlices = 512;
    constexpr std::size_t maxImagePixels = 1024 * 1024;

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

    struct Point
    {
        std::int32_t x;
        std::int32_t y;
    };
    struct Rect
    {
        std::int32_t left;
        std::int32_t top;
        std::int32_t right;
        std::int32_t bottom;
    };
    struct PointSpan
    {
        Point start;
        Point end;
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
        std::int32_t columns;
        std::int32_t padding;
    };
    static_assert(sizeof(Cluster) == 24);

    struct CursorOptions
    {
        Point position;
        std::int32_t viewportLeft;
        std::uint8_t lineRendition;
        std::uint8_t pad0[3];
        std::uint32_t heightPercent;
        std::uint32_t pixelWidth;
        bool doubleWidth;
        std::uint8_t pad1[3];
        std::uint32_t type;
        bool useColor;
        std::uint8_t pad2[3];
        COLORREF color;
        bool visible;
        bool on;
        bool inViewport;
        std::uint8_t pad3;
    };
    static_assert(sizeof(CursorOptions) == 44);

    struct OpaqueFrameInfo
    {
        std::byte bytes[48];
    };
    struct FrameInfoAbi
    {
        AbiSpan<const PointSpan> searchHighlights;
        const PointSpan* focusedSearchHighlight;
        AbiSpan<const PointSpan> selectionSpans;
        std::uint32_t selectionBackground;
        std::uint32_t padding;
    };
    static_assert(sizeof(FrameInfoAbi) == sizeof(OpaqueFrameInfo));
    struct Pixel
    {
        std::uint8_t blue;
        std::uint8_t green;
        std::uint8_t red;
        std::uint8_t alpha;
    };
    struct ImageSliceAbi
    {
        std::uint64_t revision;
        Point cellSize;
        const Pixel* first;
        const Pixel* last;
        const Pixel* capacity;
        std::int32_t columnBegin;
        std::int32_t columnEnd;
        std::int32_t pixelWidth;
        std::int32_t padding;
    };
    static_assert(sizeof(ImageSliceAbi) == 56);
    struct OpaqueImageSlice
    {
        std::byte byte;
    };
    struct Opaque
    {
        std::byte byte;
    };

    // Only vtable order and parameter ABI matter. These declarations mirror
    // IRenderEngine.hpp in WT v1.24 without linking against terminal internals.
    class RenderDataAbi
    {
    public:
        virtual ~RenderDataAbi() = default; // slot 0
        virtual void slot01() = 0; virtual void slot02() = 0; virtual void slot03() = 0;
        virtual void slot04() = 0; virtual void slot05() = 0; virtual void slot06() = 0;
        virtual void slot07() = 0;
        virtual void LockConsole() noexcept = 0; // slot 8
        virtual void UnlockConsole() noexcept = 0; // slot 9
        virtual void slot10() = 0; virtual void slot11() = 0; virtual void slot12() = 0;
        virtual void slot13() = 0; virtual void slot14() = 0; virtual void slot15() = 0;
        virtual void slot16() = 0; virtual void slot17() = 0; virtual void slot18() = 0;
        virtual std::wstring GetHyperlinkUri(std::uint16_t) const = 0; // slot 19
        virtual void slot20() = 0; virtual void slot21() = 0;
        virtual std::pair<COLORREF, COLORREF> GetAttributeColors(const Opaque&) const noexcept = 0; // slot 22
    };

    class RenderEngine
    {
    public:
        virtual ~RenderEngine() = default;
        virtual HRESULT StartPaint() noexcept = 0;
        virtual HRESULT EndPaint() noexcept = 0;
        virtual bool RequiresContinuousRedraw() noexcept = 0;
        virtual void WaitUntilCanRender() noexcept = 0;
        virtual HRESULT Present() noexcept = 0;
        virtual HRESULT ScrollFrame() noexcept = 0;
        virtual HRESULT Invalidate(const Rect*) noexcept = 0;
        virtual HRESULT InvalidateCursor(const Rect*) noexcept = 0;
        virtual HRESULT InvalidateSystem(const Rect*) noexcept = 0;
        virtual HRESULT InvalidateSelection(AbiSpan<const Rect>) noexcept = 0;
        virtual HRESULT InvalidateHighlight(AbiSpan<const void>, const Opaque&) noexcept = 0;
        virtual HRESULT InvalidateScroll(const Point*) noexcept = 0;
        virtual HRESULT InvalidateAll() noexcept = 0;
        virtual HRESULT InvalidateTitle(std::wstring_view) noexcept = 0;
        virtual HRESULT NotifyNewText(std::wstring_view) noexcept = 0;
        virtual HRESULT PrepareRenderInfo(OpaqueFrameInfo) noexcept = 0;
        virtual HRESULT ResetLineTransform() noexcept = 0;
        virtual HRESULT PrepareLineTransform(std::uint8_t, std::int32_t, std::int32_t) noexcept = 0;
        virtual HRESULT PaintBackground() noexcept = 0;
        virtual HRESULT PaintBufferLine(AbiSpan<const Cluster>, Point, bool) noexcept = 0;
        virtual HRESULT PaintBufferGridLines(std::uint64_t, COLORREF, COLORREF, std::size_t, Point) noexcept = 0;
        virtual HRESULT PaintImageSlice(const OpaqueImageSlice&, std::int32_t, std::int32_t) noexcept = 0;
        virtual HRESULT PaintSelection(const Rect&) noexcept = 0;
        virtual HRESULT PaintCursor(const CursorOptions&) noexcept = 0;
        virtual HRESULT UpdateDrawingBrushes(const Opaque&, const Opaque&, void*, bool, bool) noexcept = 0;
        virtual HRESULT UpdateFont(const Opaque&, Opaque&) noexcept = 0;
        virtual HRESULT UpdateSoftFont(AbiSpan<const std::uint16_t>, Point, std::size_t) noexcept = 0;
        virtual HRESULT UpdateDpi(int) noexcept = 0;
        virtual HRESULT UpdateViewport(const Rect&) noexcept = 0;
        virtual HRESULT GetProposedFont(const Opaque&, Opaque&, int) noexcept = 0;
        virtual HRESULT GetDirtyArea(AbiSpan<const Rect>&) noexcept = 0;
        virtual HRESULT GetFontSize(Point*) noexcept = 0;
        virtual HRESULT IsGlyphWideByFont(std::wstring_view, bool*) noexcept = 0;
        virtual HRESULT UpdateTitle(std::wstring_view) noexcept = 0;
        virtual void UpdateHyperlinkHoveredId(std::uint16_t) noexcept = 0;
    };

    struct Style
    {
        COLORREF foreground = RGB(255, 255, 255);
        COLORREF background = RGB(0, 0, 0);
        std::uint16_t flags = 0;
        std::uint8_t underline = 0;
        COLORREF underlineColor = RGB(255, 255, 255);
        std::uint16_t hyperlink = 0;
        bool operator==(const Style&) const = default;
    };
    struct ClusterEvent
    {
        std::uint16_t textLength;
        std::uint16_t styleIndex;
        std::uint8_t width;
    };
    static_assert(sizeof(ClusterEvent) == 6);
    struct LineEvent
    {
        std::uint32_t firstCluster;
        std::uint32_t firstText;
        std::uint32_t clusterCount;
        std::uint16_t row;
        std::uint16_t firstColumn;
    };
    struct ImageEvent
    {
        std::int32_t targetRow;
        std::int32_t viewportLeft;
        std::uint64_t revision;
        Point cellSize;
        std::int32_t columnBegin;
        std::int32_t columnEnd;
        std::int32_t pixelWidth;
        std::uint32_t pixelOffset;
        std::uint32_t pixelCount;
    };
    struct Batch
    {
        std::array<LineEvent, maxLines> lines{};
        std::array<ClusterEvent, maxClusters> clusters{};
        std::array<wchar_t, maxTextUnits> text{};
        std::array<wchar_t, maxTitleUnits> title{};
        std::array<Rect, maxSelections> selections{};
        std::array<PointSpan, maxSelections> searchHighlights{};
        std::array<Style, maxCaptureStyles> styles{};
        std::array<ImageEvent, maxImageSlices> images{};
        std::array<Pixel, maxImagePixels> imagePixels{};
        PointSpan focusedSearch{};
        std::uint32_t lineCount = 0;
        std::uint32_t clusterCount = 0;
        std::uint32_t textLength = 0;
        std::uint32_t imagePixelCount = 0;
        std::uint16_t imageCount = 0;
        std::uint16_t titleLength = 0;
        std::uint16_t selectionCount = 0;
        std::uint16_t searchCount = 0;
        std::uint16_t styleCount = 1;
        bool hasFocusedSearch = false;
        std::int32_t viewportLeft = 0;
        std::int32_t viewportTop = 0;
        // Even values identify a stable viewport layout. Dimension-changing
        // UpdateViewport calls use the intervening odd value.
        std::uint64_t viewportVersion = 0;
        std::uint16_t rows = 0;
        std::uint16_t cols = 0;
        COLORREF defaultForeground = RGB(255, 255, 255);
        COLORREF defaultBackground = RGB(0, 0, 0);
        COLORREF selectionBackground = RGB(255, 255, 255);
        bool hasCursor = false;
        CursorOptions cursor{};
        Rect dirtyArea{};
        bool fullRepaint = true;
        bool overflow = false;

        void reset(const std::uint16_t newRows, const std::uint16_t newCols,
                   const std::uint64_t newViewportVersion) noexcept
        {
            lineCount = clusterCount = textLength = imagePixelCount = titleLength = selectionCount = searchCount = 0;
            imageCount = 0;
            styleCount = 1;
            styles[0] = {};
            rows = newRows;
            cols = newCols;
            viewportVersion = newViewportVersion;
            hasCursor = false;
            hasFocusedSearch = false;
            dirtyArea = { 0, 0, newCols, newRows };
            fullRepaint = true;
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
        imageBlob = 6,
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
        {
            out.push_back(static_cast<std::uint8_t>(static_cast<U>(value) >> (8 * i)));
        }
    }
    void appendString(std::vector<std::uint8_t>& out, const std::string_view value)
    {
        append(out, static_cast<std::uint16_t>((std::min)(value.size(), static_cast<std::size_t>(UINT16_MAX))));
        out.insert(out.end(), value.begin(), value.begin() + (std::min)(value.size(), static_cast<std::size_t>(UINT16_MAX)));
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
    bool sendPacket(HANDLE pipe, MessageType type, std::uint64_t nonce, std::uint64_t sequence, const std::vector<std::uint8_t>& payload)
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
                  BCryptGetProperty(algorithm, BCRYPT_OBJECT_LENGTH, reinterpret_cast<PUCHAR>(&objectLength), sizeof(objectLength), &resultLength, 0) >= 0;
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
        if (length == 0 || length == path.size()) return false;
        std::filesystem::path profilePath{ std::wstring_view{ path.data(), length } };
        profilePath.replace_extension(L".sgnp");
        std::ifstream input{ profilePath, std::ios::binary };
        std::vector<std::uint8_t> bytes{ std::istreambuf_iterator<char>{ input }, {} };
        if (bytes.size() < sizeof(ProfileHeader) + 32) return false;
        std::array<std::uint8_t, 32> digest{};
        if (!sha256(std::span{ bytes }.first(bytes.size() - digest.size()), digest.data()) ||
            std::memcmp(digest.data(), bytes.data() + bytes.size() - digest.size(), digest.size()) != 0)
        {
            return false;
        }
        std::memcpy(&profile, bytes.data(), sizeof(profile));
        const auto family = std::string_view{ profile.family, strnlen_s(profile.family, sizeof(profile.family)) };
        if (std::memcmp(profile.magic, "SGNP", 4) != 0 || profile.version != 1 ||
            profile.machine != IMAGE_FILE_MACHINE_AMD64 ||
            (family != "wt_1_24" && family != "wt_1_24_11321") ||
            profile.entryCount != 7 ||
            bytes.size() != sizeof(profile) + profile.entryCount * sizeof(ProfileEntry) + 32)
        {
            return false;
        }
        const auto* entries = reinterpret_cast<const ProfileEntry*>(bytes.data() + sizeof(profile));
        for (std::uint32_t i = 0; i < profile.entryCount; ++i)
        {
            if (!profileEntries.emplace(entries[i].id, entries[i]).second) return false;
        }
        return profileEntries.contains(1) && profileEntries.contains(2) && profileEntries.contains(3) &&
               profileEntries.contains(4) && profileEntries.contains(8) && profileEntries.contains(10) &&
               profileEntries.contains(11);
    }

    bool verifyModule(HMODULE module)
    {
        const auto* base = reinterpret_cast<const std::uint8_t*>(module);
        const auto* dos = reinterpret_cast<const IMAGE_DOS_HEADER*>(base);
        if (dos->e_magic != IMAGE_DOS_SIGNATURE) return false;
        const auto* nt = reinterpret_cast<const IMAGE_NT_HEADERS64*>(base + dos->e_lfanew);
        if (nt->Signature != IMAGE_NT_SIGNATURE || nt->FileHeader.Machine != profile.machine ||
            nt->OptionalHeader.SizeOfImage != profile.imageSize)
        {
            return false;
        }
        bool pdbMatched = false;
        const auto& debugDirectory = nt->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_DEBUG];
        if (debugDirectory.VirtualAddress && debugDirectory.Size >= sizeof(IMAGE_DEBUG_DIRECTORY))
        {
            const auto* entries = reinterpret_cast<const IMAGE_DEBUG_DIRECTORY*>(base + debugDirectory.VirtualAddress);
            for (std::size_t i = 0; i < debugDirectory.Size / sizeof(*entries); ++i)
            {
                if (entries[i].Type != IMAGE_DEBUG_TYPE_CODEVIEW || entries[i].SizeOfData < 24) continue;
                const auto* codeView = base + entries[i].AddressOfRawData;
                GUID guid{};
                std::uint32_t age = 0;
                if (std::memcmp(codeView, "RSDS", 4) == 0)
                {
                    std::memcpy(&guid, codeView + 4, sizeof(guid));
                    std::memcpy(&age, codeView + 20, sizeof(age));
                    pdbMatched = std::memcmp(&guid, &profile.pdbGuid, sizeof(guid)) == 0 && age == profile.pdbAge;
                    if (pdbMatched) break;
                }
            }
        }
        if (!pdbMatched) return false;
        std::array<wchar_t, 32768> path{};
        const auto length = GetModuleFileNameW(module, path.data(), static_cast<DWORD>(path.size()));
        std::ifstream file{ std::filesystem::path{ std::wstring_view{ path.data(), length } }, std::ios::binary };
        std::vector<std::uint8_t> bytes{ std::istreambuf_iterator<char>{ file }, {} };
        if (bytes.empty() || !sha256(bytes, moduleHash.data()) || moduleHash != std::to_array(profile.moduleSha256)) return false;
        for (const auto& [id, entry] : profileEntries)
        {
            (void)id;
            if (entry.rva > profile.imageSize || sizeof(entry.expected) > profile.imageSize - entry.rva ||
                std::memcmp(base + entry.rva, entry.expected, sizeof(entry.expected)) != 0)
            {
                return false;
            }
        }
        return true;
    }

    // Build a trampoline only for prologues without relative addressing. The
    // wt_1_24 owner setter's RIP-relative cookie load has a dedicated rewrite
    // below; truncating an arbitrary relocation to rel32 can crash when the
    // trampoline allocation lands outside +/-2 GiB of the image.
    void* absoluteTrampoline(void* target, std::size_t stolen)
    {
        auto* memory = static_cast<std::uint8_t*>(VirtualAlloc(nullptr, stolen + 14, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE));
        if (!memory) return nullptr;
        std::memcpy(memory, target, stolen);
        memory[stolen] = 0xff;
        memory[stolen + 1] = 0x25;
        std::memset(memory + stolen + 2, 0, 4);
        const auto continuation = reinterpret_cast<std::uintptr_t>(target) + stolen;
        std::memcpy(memory + stolen + 6, &continuation, sizeof(continuation));
        DWORD oldProtect = 0;
        if (!VirtualProtect(memory, stolen + 14, PAGE_EXECUTE_READ, &oldProtect)) return nullptr;
        FlushInstructionCache(GetCurrentProcess(), memory, stolen + 14);
        return memory;
    }

    bool patchHook(void* target, void* hook, std::size_t stolen)
    {
        if (stolen < 14) return false;
        DWORD oldProtect = 0;
        if (!VirtualProtect(target, stolen, PAGE_EXECUTE_READWRITE, &oldProtect)) return false;
        auto* bytes = static_cast<std::uint8_t*>(target);
        bytes[0] = 0xff;
        bytes[1] = 0x25;
        std::memset(bytes + 2, 0, 4);
        std::memcpy(bytes + 6, &hook, sizeof(hook));
        std::memset(bytes + 14, 0x90, stolen - 14);
        FlushInstructionCache(GetCurrentProcess(), target, stolen);
        DWORD ignored = 0;
        return VirtualProtect(target, stolen, oldProtect, &ignored) != 0;
    }

    class CaptureEngine;
    std::mutex enginesMutex;
    std::vector<std::unique_ptr<CaptureEngine>> engines;
    std::unordered_map<void*, CaptureEngine*> coreEngines;
    std::unordered_map<void*, std::uint64_t> coreOwners;
    std::unordered_map<void*, bool> coreFocus;
    std::unordered_set<void*> recoveringCores;
    thread_local void* initializingCore = nullptr;

    // Family-pinned by validateWt124Abi. These are consumed only after the PE,
    // RSDS identity, hash, profile integrity, and complete type layout all match.
    constexpr std::size_t coreOwnerOffset = 896;
    constexpr std::size_t coreRendererOffset = 1192;
    constexpr std::size_t rendererDataOffset = 24;

    using InitializeFn = bool(__fastcall*)(void*, float, float, float);
    using DestructorFn = void(__fastcall*)(void*);
    using FocusFn = void(__fastcall*)(void*, bool);
    using OwnerFn = void(__fastcall*)(void*, std::uint64_t);
    using AddFn = void(__fastcall*)(void*, RenderEngine*);
    using TriggerFn = void(__fastcall*)(void*, bool, bool);
    using UnderlineColorFn = COLORREF(__fastcall*)(const Opaque*, const Opaque*);
    InitializeFn originalInitialize = nullptr;
    DestructorFn originalDestructor = nullptr;
    FocusFn originalFocus = nullptr;
    OwnerFn originalOwner = nullptr;
    AddFn originalAdd = nullptr;
    TriggerFn triggerRedraw = nullptr;
    UnderlineColorFn underlineColor = nullptr;

    class CaptureEngine final : public RenderEngine
    {
    public:
        explicit CaptureEngine(void* renderer) : renderer{ renderer }
        {
            QueryPerformanceFrequency(&performanceFrequency);
            source = reinterpret_cast<std::uint64_t>(this);
        }

        HRESULT StartPaint() noexcept override
        {
            // This counter brackets the entire renderer paint transaction. Its
            // sequentially-consistent ordering with `subscribed` lets the worker
            // reclaim large dormant buffers without racing a callback that read
            // the old subscription state.
            activePaint.fetch_add(1);
            if (!alive.load(std::memory_order_acquire) || !subscribed.load() ||
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
            // UpdateViewport may run independently of invalidation delivery.
            // Snapshot dimensions under a small seqlock instead of combining
            // rows and columns from two resize generations in one paint batch.
            const auto version = viewportVersion.load(std::memory_order_acquire);
            if ((version & 1) != 0)
            {
                dirty.store(true, std::memory_order_release);
                activePaint.fetch_sub(1);
                return S_FALSE;
            }
            const auto paintRows = rows.load(std::memory_order_acquire);
            const auto paintCols = cols.load(std::memory_order_acquire);
            if (version != viewportVersion.load(std::memory_order_acquire))
            {
                dirty.store(true, std::memory_order_release);
                activePaint.fetch_sub(1);
                return S_FALSE;
            }
            current->reset(paintRows, paintCols, version);
            captureStyleIndex = 0;
            QueryPerformanceCounter(&paintStarted);
            painting = true;
            return S_OK;
        }
        HRESULT EndPaint() noexcept override
        {
            painting = false;
            LARGE_INTEGER ended{};
            QueryPerformanceCounter(&ended);
            const auto micros = static_cast<std::uint64_t>((ended.QuadPart - paintStarted.QuadPart) * 1'000'000 / performanceFrequency.QuadPart);
            const auto bucket = micros <= 250 ? 0u : micros <= 500 ? 1u : micros <= 1000 ? 2u : micros <= 2000 ? 3u : 4u;
            performanceBuckets[bucket].fetch_add(1, std::memory_order_relaxed);
            auto maximum = performanceMaxMicros.load(std::memory_order_relaxed);
            while (micros > maximum && !performanceMaxMicros.compare_exchange_weak(maximum, micros, std::memory_order_relaxed)) {}
            // Publish count last so an acquiring worker sees the matching bucket.
            performanceCount.fetch_add(1, std::memory_order_release);
            if (!current || current->overflow ||
                current->viewportVersion != viewportVersion.load(std::memory_order_acquire) ||
                current->rows != rows.load(std::memory_order_acquire) ||
                current->cols != cols.load(std::memory_order_acquire))
            {
                // A resize that overlaps this render transaction can otherwise
                // pair old row runs with the new dimensions. Drop it and demand
                // a new full presentation from one stable viewport generation.
                markFullDirty();
                activePaint.fetch_sub(1);
                return S_OK;
            }
            // These batches contain dirty-region deltas, not complete terminal
            // snapshots. Replacing an unconsumed batch can therefore lose cells
            // that a later partial repaint does not revisit (rapid TUIs such as
            // Claude Code expose this as missing characters). Keep the older
            // pending delta, drop this newer one, and ask the worker to schedule
            // one full reconciliation as soon as it frees the pending slot.
            Batch* expected = nullptr;
            if (pending.compare_exchange_strong(expected, current, std::memory_order_acq_rel,
                                                std::memory_order_acquire))
            {
                current = acquireFreeBatch();
            }
            else
            {
                droppedFrames.fetch_add(1, std::memory_order_relaxed);
                reconcileAfterDrop.store(true, std::memory_order_release);
            }
            activePaint.fetch_sub(1);
            return S_OK;
        }
        bool RequiresContinuousRedraw() noexcept override
        {
            // If a viewport update invalidated the transaction in EndPaint, ask
            // WT's renderer to run the forced full paint we left pending.
            return dirty.load(std::memory_order_acquire);
        }
        void WaitUntilCanRender() noexcept override {}
        HRESULT Present() noexcept override { return S_OK; }
        HRESULT ScrollFrame() noexcept override { return S_OK; }
        HRESULT Invalidate(const Rect* rect) noexcept override { addDirty(rect); return S_OK; }
        HRESULT InvalidateCursor(const Rect* rect) noexcept override { addDirty(rect); return S_OK; }
        HRESULT InvalidateSystem(const Rect*) noexcept override { markFullDirty(); return S_OK; }
        HRESULT InvalidateSelection(const AbiSpan<const Rect> areas) noexcept override
        {
            for (std::size_t i = 0; i < areas.size; ++i) addDirty(areas.data + i);
            return S_OK;
        }
        HRESULT InvalidateHighlight(AbiSpan<const void>, const Opaque&) noexcept override { markFullDirty(); return S_OK; }
        HRESULT InvalidateScroll(const Point*) noexcept override { markFullDirty(); return S_OK; }
        HRESULT InvalidateAll() noexcept override { markFullDirty(); return S_OK; }
        HRESULT InvalidateTitle(std::wstring_view title) noexcept override { copyTitle(title); markFullDirty(); return S_OK; }
        HRESULT NotifyNewText(std::wstring_view) noexcept override { markDirty(); return S_OK; }
        HRESULT PrepareRenderInfo(OpaqueFrameInfo info) noexcept override
        {
            if (painting && current)
            {
                const auto& decoded = reinterpret_cast<const FrameInfoAbi&>(info);
                current->selectionBackground = static_cast<COLORREF>(decoded.selectionBackground & 0x00ffffffu);
                current->viewportLeft = viewportLeft.load(std::memory_order_acquire);
                current->viewportTop = viewportTop.load(std::memory_order_acquire);
                current->searchCount = static_cast<std::uint16_t>((std::min)(decoded.searchHighlights.size, maxSelections));
                if (current->searchCount)
                    std::memcpy(current->searchHighlights.data(), decoded.searchHighlights.data,
                                current->searchCount * sizeof(PointSpan));
                if (decoded.focusedSearchHighlight)
                {
                    current->focusedSearch = *decoded.focusedSearchHighlight;
                    current->hasFocusedSearch = true;
                }
            }
            return S_OK;
        }
        HRESULT ResetLineTransform() noexcept override { return S_OK; }
        HRESULT PrepareLineTransform(std::uint8_t, std::int32_t, std::int32_t) noexcept override { return S_OK; }
        HRESULT PaintBackground() noexcept override { return S_OK; }
        HRESULT PaintBufferLine(AbiSpan<const Cluster> span, Point point, bool) noexcept override
        {
#if defined(SHELLGLASS_CALLBACK_FAULT_TEST)
            // Test-only binary: exercise the production failure-containment path
            // from a real stock-WT render callback without corrupting the target.
            if (!faulted.load(std::memory_order_acquire))
            {
                disableCapture();
                return S_OK;
            }
#endif
            if (!painting || !current || point.y < 0 || point.y >= 500 || span.size > maxClusters) return S_OK;
            if (current->lineCount >= maxLines || span.size > maxClusters - current->clusterCount)
            {
                current->overflow = true;
                return S_OK;
            }
            auto& line = current->lines[current->lineCount++];
            line.row = static_cast<std::uint16_t>(point.y);
            line.firstColumn = static_cast<std::uint16_t>((std::max)(point.x, 0));
            line.firstCluster = current->clusterCount;
            line.firstText = current->textLength;
            line.clusterCount = 0;
            auto column = point.x;
            for (std::size_t i = 0; i < span.size; ++i)
            {
                const auto& input = span.data[i];
                if (column < 0 || column >= 1000 || input.columns < 1 || input.columns > 2)
                {
                    current->overflow = true;
                    break;
                }
                if (input.length > maxTextUnits - current->textLength)
                {
                    current->overflow = true;
                    break;
                }
                auto& output = current->clusters[current->clusterCount++];
                output.width = static_cast<std::uint8_t>(input.columns);
                output.textLength = static_cast<std::uint16_t>((std::min)(input.length, static_cast<std::size_t>(UINT16_MAX)));
                output.styleIndex = captureStyleIndex;
                if (output.textLength == 1)
                {
                    current->text[current->textLength++] = *input.text;
                }
                else if (output.textLength != 0)
                {
                    std::memcpy(current->text.data() + current->textLength, input.text, output.textLength * sizeof(wchar_t));
                    current->textLength += output.textLength;
                }
                ++line.clusterCount;
                column += input.columns;
            }
            const auto observedRows = static_cast<std::uint16_t>(point.y + 1);
            const auto observedCols = static_cast<std::uint16_t>((std::min)((std::max)(column, 1), 1000));
            current->rows = (std::max)(current->rows, observedRows);
            current->cols = (std::max)(current->cols, observedCols);
            const auto rowsChanged = grow(rows, current->rows);
            const auto colsChanged = grow(cols, current->cols);
            if (rowsChanged || colsChanged) metadataChanged.store(true, std::memory_order_release);
            return S_OK;
        }
        HRESULT PaintBufferGridLines(std::uint64_t, COLORREF, COLORREF, std::size_t, Point) noexcept override { return S_OK; }
        HRESULT PaintImageSlice(const OpaqueImageSlice& opaque, std::int32_t targetRow, std::int32_t viewportLeftValue) noexcept override
        {
            if (!painting || !current) return S_OK;
            const auto& slice = reinterpret_cast<const ImageSliceAbi&>(opaque);
            if (slice.cellSize.x <= 0 || slice.cellSize.y <= 0 || slice.pixelWidth <= 0 ||
                slice.columnEnd <= slice.columnBegin || !slice.first || !slice.last)
                return S_OK;
            const auto firstAddress = reinterpret_cast<std::uintptr_t>(slice.first);
            const auto lastAddress = reinterpret_cast<std::uintptr_t>(slice.last);
            if (lastAddress < firstAddress || (lastAddress - firstAddress) % sizeof(Pixel) != 0) return S_OK;
            const auto expected = static_cast<std::uint64_t>(slice.pixelWidth) * static_cast<std::uint64_t>(slice.cellSize.y);
            const auto available = static_cast<std::uint64_t>((lastAddress - firstAddress) / sizeof(Pixel));
            if (expected == 0 || expected > available || expected > maxImagePixels - current->imagePixelCount ||
                current->imageCount >= current->images.size())
            {
                current->overflow = true;
                return S_OK;
            }
            auto& image = current->images[current->imageCount++];
            image = { targetRow, viewportLeftValue, slice.revision, slice.cellSize,
                      slice.columnBegin, slice.columnEnd, slice.pixelWidth,
                      current->imagePixelCount, static_cast<std::uint32_t>(expected) };
            std::memcpy(current->imagePixels.data() + current->imagePixelCount, slice.first,
                        static_cast<std::size_t>(expected) * sizeof(Pixel));
            current->imagePixelCount += static_cast<std::uint32_t>(expected);
            return S_OK;
        }
        HRESULT PaintSelection(const Rect& rect) noexcept override
        {
            if (painting && current)
            {
                if (current->selectionCount < current->selections.size())
                    current->selections[current->selectionCount++] = rect;
                else
                    current->overflow = true;
            }
            return S_OK;
        }
        HRESULT PaintCursor(const CursorOptions& options) noexcept override
        {
            if (painting && current)
            {
                current->hasCursor = true;
                current->cursor = options;
            }
            return S_OK;
        }
        __declspec(guard(nocf)) HRESULT UpdateDrawingBrushes(const Opaque& attributes, const Opaque& settings, void* data, bool, bool defaults) noexcept override
        {
            const auto* raw = reinterpret_cast<const std::uint8_t*>(&attributes);
            const auto attrs = *reinterpret_cast<const std::uint16_t*>(raw);
            const auto hyperlink = *reinterpret_cast<const std::uint16_t*>(raw + 2);
            COLORREF foreground = RGB(255, 255, 255);
            COLORREF background = RGB(0, 0, 0);
            if (data)
            {
                renderData.store(reinterpret_cast<RenderDataAbi*>(data), std::memory_order_release);
                // Use a virtual member call so MSVC places `this`, the hidden
                // std::pair return buffer, and the attribute reference exactly as
                // WT's member-function ABI requires. A free-function cast shifts
                // those registers and crashes inside RenderSettings.
                const auto colors = reinterpret_cast<RenderDataAbi*>(data)->GetAttributeColors(attributes);
                foreground = colors.first;
                background = colors.second;
            }
            style = {};
            style.foreground = foreground;
            style.background = background;
            style.hyperlink = hyperlink;
            style.underlineColor = underlineColor ? underlineColor(&settings, &attributes) : foreground;
            if (attrs & 0x01) style.flags |= 0x01;
            if (attrs & 0x20) style.flags |= 0x02;
            if (attrs & 0x02) style.flags |= 0x04;
            if (attrs & 0x10) style.flags |= 0x08;
            if (attrs & 0x08) style.flags |= 0x10;
            if (attrs & 0x04) style.flags |= 0x20;
            if (attrs & 0x4000) style.flags |= 0x40;
            style.underline = static_cast<std::uint8_t>((attrs & 0x1c0) >> 6);
            if (painting && current)
            {
                if (current->styleCount >= current->styles.size())
                {
                    current->overflow = true;
                }
                else if (!(current->styles[current->styleCount - 1] == style))
                {
                    captureStyleIndex = current->styleCount;
                    current->styles[current->styleCount++] = style;
                }
                else
                {
                    captureStyleIndex = static_cast<std::uint16_t>(current->styleCount - 1);
                }
            }
            if (defaults && painting && current)
            {
                current->defaultForeground = foreground;
                current->defaultBackground = background;
            }
            return S_OK;
        }
        HRESULT UpdateFont(const Opaque&, Opaque&) noexcept override { return S_OK; }
        HRESULT UpdateSoftFont(AbiSpan<const std::uint16_t>, Point, std::size_t) noexcept override { return S_OK; }
        HRESULT UpdateDpi(int) noexcept override { return S_OK; }
        HRESULT UpdateViewport(const Rect& viewport) noexcept override
        {
            const auto width = viewport.right - viewport.left + 1;
            const auto height = viewport.bottom - viewport.top + 1;
            if (width > 0 && width <= 1000 && height > 0 && height <= 500)
            {
                const auto newCols = static_cast<std::uint16_t>(width);
                const auto newRows = static_cast<std::uint16_t>(height);
                if (cols.load(std::memory_order_acquire) == newCols &&
                    rows.load(std::memory_order_acquire) == newRows &&
                    viewportLeft.load(std::memory_order_acquire) == viewport.left &&
                    viewportTop.load(std::memory_order_acquire) == viewport.top)
                    return S_OK;

                const auto dimensionsChanged =
                    cols.load(std::memory_order_relaxed) != newCols ||
                    rows.load(std::memory_order_relaxed) != newRows;
                // Bracket dimension changes with an odd/even sequence. Scroll
                // position alone must not obsolete a completed image batch:
                // ordinary output advances viewportTop continuously, and the
                // next ordered paint naturally catches that up.
                if (dimensionsChanged)
                {
                    layoutChanges.fetch_add(1, std::memory_order_relaxed);
                    viewportVersion.fetch_add(1, std::memory_order_acq_rel);
                }
                cols.store(newCols, std::memory_order_release);
                rows.store(newRows, std::memory_order_release);
                viewportLeft.store(viewport.left, std::memory_order_release);
                viewportTop.store(viewport.top, std::memory_order_release);
                if (dimensionsChanged)
                    viewportVersion.fetch_add(1, std::memory_order_release);
                metadataChanged.store(true, std::memory_order_release);
                markFullDirty();
            }
            return S_OK;
        }
        HRESULT GetProposedFont(const Opaque&, Opaque&, int) noexcept override { return E_NOTIMPL; }
        HRESULT GetDirtyArea(AbiSpan<const Rect>& area) noexcept override
        {
            const auto full = forceFullDirty.exchange(false, std::memory_order_acq_rel) || !hasDirtyArea;
            if (full)
                dirtyArea = { 0, 0, cols.load(std::memory_order_acquire), rows.load(std::memory_order_acquire) };
            hasDirtyArea = false;
            if (painting && current)
            {
                current->dirtyArea = dirtyArea;
                current->fullRepaint = full;
            }
            area = { &dirtyArea, 1 };
            return S_OK;
        }
        HRESULT GetFontSize(Point* size) noexcept override
        {
            if (size) *size = { 1, 1 };
            return S_OK;
        }
        HRESULT IsGlyphWideByFont(std::wstring_view, bool* result) noexcept override
        {
            if (result) *result = false;
            return S_OK;
        }
        HRESULT UpdateTitle(std::wstring_view title) noexcept override { copyTitle(title); return S_OK; }
        void UpdateHyperlinkHoveredId(std::uint16_t) noexcept override {}

        void markDirty() noexcept { dirty.store(true, std::memory_order_release); }
        void markFullDirty() noexcept
        {
            forceFullDirty.store(true, std::memory_order_release);
            markDirty();
        }
        void addDirty(const Rect* rect) noexcept
        {
            if (!rect || rect->right <= rect->left || rect->bottom <= rect->top)
            {
                markFullDirty();
                return;
            }
            if (!hasDirtyArea)
            {
                dirtyArea = *rect;
                hasDirtyArea = true;
            }
            else
            {
                dirtyArea.left = (std::min)(dirtyArea.left, rect->left);
                dirtyArea.top = (std::min)(dirtyArea.top, rect->top);
                dirtyArea.right = (std::max)(dirtyArea.right, rect->right);
                dirtyArea.bottom = (std::max)(dirtyArea.bottom, rect->bottom);
            }
            markDirty();
        }
        // These exact private targets are validated by module hash, PDB identity,
        // RVA, and prologue. They are absent from WT's GFID table, so only this
        // narrow indirect call site opts out of compiler CFG instrumentation.
        __declspec(guard(nocf)) void requestFull() noexcept
        {
            if (!current)
            {
                try
                {
                    for (auto& item : batches) item = std::make_unique<Batch>();
                }
                catch (...)
                {
                    disableCapture();
                    return;
                }
                current = batches[0].get();
                free.store(batches[1].get(), std::memory_order_release);
                spare.store(batches[2].get(), std::memory_order_release);
            }
            releaseRequested.store(false, std::memory_order_release);
            reconcileAfterDrop.store(false, std::memory_order_release);
            subscribed.store(true);
            markFullDirty();
            triggerFullRedraw();
        }
        void disableCapture() noexcept
        {
            // Render callbacks only publish atomics here. Diagnostics and source
            // removal are emitted by the worker, never from the target thread.
            faulted.store(true, std::memory_order_release);
            park();
            dirty.store(false, std::memory_order_release);
            painting = false;
            if (current) current->overflow = true;
            alive.store(false, std::memory_order_release);
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
            reconcileAfterDrop.store(false, std::memory_order_release);
            free.store(nullptr, std::memory_order_release);
            spare.store(nullptr, std::memory_order_release);
            for (auto& batch : batches) batch.reset();
            matrix.clear();
            matrix.shrink_to_fit();
            modelRows = 0;
            modelCols = 0;
            modelImages.clear();
            modelImages.shrink_to_fit();
            releaseRequested.store(false, std::memory_order_release);
        }
        Batch* takeBatch() noexcept
        {
            auto* batch = pending.exchange(nullptr, std::memory_order_acq_rel);
            if (batch && reconcileAfterDrop.exchange(false, std::memory_order_acq_rel))
            {
                // At least one newer delta was dropped, so applying this older
                // delta would expose a knowingly incomplete intermediate model.
                // Keep the last published model coherent and replace the queued
                // delta with one full repaint instead.
                releaseBatch(batch);
                markFullDirty();
                triggerFullRedraw();
                return nullptr;
            }
            return batch;
        }
        void releaseBatch(Batch* batch) noexcept
        {
            Batch* expected = nullptr;
            if (free.compare_exchange_strong(expected, batch, std::memory_order_release, std::memory_order_relaxed)) return;
            expected = nullptr;
            if (!spare.compare_exchange_strong(expected, batch, std::memory_order_release, std::memory_order_relaxed))
            {
                // Three batches can only be current, pending/worker-owned, and
                // free/spare. Both return slots being occupied is an invariant
                // violation, so fail only this provider rather than race or wait.
                disableCapture();
            }
        }
        void copyTitle(std::wstring_view title) noexcept
        {
            if (!painting || !current) return;
            current->titleLength = static_cast<std::uint16_t>((std::min)(title.size(), maxTitleUnits));
            if (current->titleLength) std::memcpy(current->title.data(), title.data(), current->titleLength * sizeof(wchar_t));
        }

        void* renderer;
        std::uint64_t source = 0;
        // The registry owns an engine before calling the target AddRenderEngine
        // so allocation failure can never leave the renderer with a dangling
        // pointer. The worker ignores it until target attachment completes.
        std::atomic<bool> attached{ false };
        std::atomic<bool> alive{ true };
        std::atomic<bool> faulted{ false };
        std::atomic<bool> focused{ true };
        std::atomic<bool> subscribed{ false };
        std::atomic<bool> releaseRequested{ false };
        std::atomic<std::uint32_t> activePaint{ 0 };
        std::atomic<bool> dirty{ true };
        std::atomic<bool> forceFullDirty{ true };
        std::atomic<bool> metadataChanged{ true };
        // AddRenderEngine does not replay the current viewport. Register a 1x1
        // provisional source to break the subscribe/redraw dependency cycle;
        // the first forced full paint infers exact viewport extents from row runs.
        std::atomic<std::uint16_t> rows{ 1 };
        std::atomic<std::uint16_t> cols{ 1 };
        std::atomic<std::int32_t> viewportLeft{ 0 };
        std::atomic<std::int32_t> viewportTop{ 0 };
        std::atomic<std::uint64_t> viewportVersion{ 0 };
        std::atomic<std::uint64_t> owner{ 0 };
        std::atomic<RenderDataAbi*> renderData{ nullptr };
        std::atomic<std::uint64_t> performanceCount{ 0 };
        std::array<std::atomic<std::uint64_t>, 5> performanceBuckets{};
        std::atomic<std::uint64_t> performanceMaxMicros{ 0 };
        std::atomic<std::uint64_t> droppedFrames{ 0 };
        std::atomic<bool> reconcileAfterDrop{ false };
        std::atomic<std::uint64_t> layoutChanges{ 0 };
        std::atomic<std::uint64_t> staleLayoutBatches{ 0 };
        // Worker-owned baselines make each diagnostic describe the latest
        // 120+ callbacks rather than letting earlier small viewports mask a
        // regression at a later matrix size.
        std::uint64_t performanceReportedCount = 0;
        std::array<std::uint64_t, 5> performanceReportedBuckets{};
        bool faultDiagnosticSent = false;
        bool sourceSent = false;
        std::uint64_t frameSequence = 1;

        struct EncodedImage
        {
            std::int16_t row = 0;
            std::uint16_t col = 0;
            float cols = 0;
            float rows = 0;
            std::string key;
            std::vector<std::uint8_t> png;
        };
        struct Cell
        {
            std::string text{ " " };
            std::uint8_t width = 1;
            bool continuation = false;
            Style style{};
        };
        std::vector<Cell> matrix;
        std::vector<EncodedImage> modelImages;
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
        // The exact TriggerRedraw target is not in WT's GFID table. Keep this
        // call in a non-inlined nocf island: MSVC otherwise inlines requestFull
        // or takeBatch into the CFG-protected worker and emits a guarded indirect
        // call, which fail-fasts precisely when drop reconciliation requests a
        // full redraw.
        __declspec(noinline) __declspec(guard(nocf)) void triggerFullRedraw() noexcept
        {
            if (alive.load(std::memory_order_acquire) && subscribed.load())
                triggerRedraw(renderer, true, true);
        }

        static bool grow(std::atomic<std::uint16_t>& value, const std::uint16_t candidate) noexcept
        {
            auto currentValue = value.load(std::memory_order_relaxed);
            while (candidate > currentValue)
            {
                if (value.compare_exchange_weak(currentValue, candidate, std::memory_order_release, std::memory_order_relaxed))
                    return true;
            }
            return false;
        }

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
        Rect dirtyArea{};
        bool hasDirtyArea = false;
        Style style{};
        std::uint16_t captureStyleIndex = 0;
    };

    bool recoverExistingCore(void* core, const bool focused) noexcept
    {
        // _focusChanged gives an authoritative live ControlCore pointer even
        // when Initialize predates injection. Recover only exact PDB-verified
        // members; never scan the heap or infer a layout from signatures.
        {
            std::scoped_lock registry{ enginesMutex };
            if (coreEngines.contains(core)) return true;
            try
            {
                if (!recoveringCores.insert(core).second) return false;
            }
            catch (...)
            {
                return false;
            }
        }

        void* renderer = nullptr;
        RenderDataAbi* data = nullptr;
        std::uint64_t owner = 0;
        std::memcpy(&renderer, static_cast<std::uint8_t*>(core) + coreRendererOffset, sizeof(renderer));
        std::memcpy(&owner, static_cast<std::uint8_t*>(core) + coreOwnerOffset, sizeof(owner));
        if (renderer)
            std::memcpy(&data, static_cast<std::uint8_t*>(renderer) + rendererDataOffset, sizeof(data));
        if (!renderer || !data)
        {
            std::scoped_lock registry{ enginesMutex };
            recoveringCores.erase(core);
            return false;
        }

        CaptureEngine* pointer = nullptr;
        try
        {
            auto capture = std::make_unique<CaptureEngine>(renderer);
            capture->owner.store(owner, std::memory_order_release);
            capture->focused.store(focused, std::memory_order_release);
            capture->renderData.store(data, std::memory_order_release);
            pointer = capture.get();
            std::scoped_lock registry{ enginesMutex };
            // Reserve before publishing the map entry. Once AddRenderEngine has
            // seen the raw pointer, the process-lifetime vector must own it.
            engines.reserve(engines.size() + 1);
            const auto inserted = coreEngines.emplace(core, pointer).second;
            if (!inserted)
            {
                recoveringCores.erase(core);
                return true;
            }
            engines.push_back(std::move(capture));
        }
        catch (...)
        {
            std::scoped_lock registry{ enginesMutex };
            recoveringCores.erase(core);
            return false;
        }

        bool locked = false;
        try
        {
            // Mirrors ControlCore::AttachUiaEngine: mutate Renderer::_engines
            // only under the terminal's authoritative IRenderData lock.
            data->LockConsole();
            locked = true;
            originalAdd(renderer, pointer);
            data->UnlockConsole();
            locked = false;
            pointer->attached.store(true, std::memory_order_release);
            pointer->metadataChanged.store(true, std::memory_order_release);
        }
        catch (...)
        {
            if (locked)
            {
                try { data->UnlockConsole(); } catch (...) {}
            }
            // The engine remains process-lifetime. Whether AddRenderEngine threw
            // before or after insertion, disabling it prevents target callbacks
            // from touching partially initialized capture state.
            pointer->alive.store(false, std::memory_order_release);
        }
        {
            std::scoped_lock registry{ enginesMutex };
            recoveringCores.erase(core);
        }
        return pointer->attached.load(std::memory_order_acquire);
    }

    __declspec(guard(nocf)) bool __fastcall hookedInitialize(void* core, float width, float height, float scale)
    {
        initializingCore = core;
        const auto result = originalInitialize(core, width, height, scale);
        initializingCore = nullptr;
        if (result)
        {
            // Some default-terminal/delegation launches initialize the first
            // ControlCore without traversing the hooked OwningHwnd setter after
            // our render engine is attached. Reconcile the final exact member
            // once Initialize returns; otherwise the source remains owner=0 and
            // the broker falls back to accessibility until another tab focuses.
            std::uint64_t owner = 0;
            std::memcpy(&owner, static_cast<std::uint8_t*>(core) + coreOwnerOffset, sizeof(owner));
            if (owner)
            {
                std::scoped_lock lock{ enginesMutex };
                if (const auto found = coreEngines.find(core); found != coreEngines.end())
                {
                    found->second->owner.store(owner, std::memory_order_release);
                    found->second->metadataChanged.store(true, std::memory_order_release);
                }
            }
        }
        return result;
    }
    __declspec(guard(nocf)) void __fastcall hookedDestructor(void* core)
    {
        {
            std::scoped_lock lock{ enginesMutex };
            recoveringCores.erase(core);
            if (const auto found = coreEngines.find(core); found != coreEngines.end())
            {
                found->second->alive.store(false);
                found->second->park();
            }
            coreOwners.erase(core);
            coreFocus.erase(core);
        }
        originalDestructor(core);
    }
    __declspec(guard(nocf)) void __fastcall hookedFocus(void* core, bool focused)
    {
        originalFocus(core, focused);
        // A loss transition is equally authoritative and lets the currently
        // focused pre-injection tab recover as soon as the user switches tabs
        // or leaves WT; the next gain transition then selects it normally.
        recoverExistingCore(core, focused);
        // Focus callbacks are authoritative live ControlCore pointers. Refresh
        // the same exact verified owner member here too, repairing any source
        // created before a delegated window had finalized its HWND.
        std::uint64_t owner = 0;
        std::memcpy(&owner, static_cast<std::uint8_t*>(core) + coreOwnerOffset, sizeof(owner));
        std::scoped_lock lock{ enginesMutex };
        coreFocus[core] = focused;
        if (const auto found = coreEngines.find(core); found != coreEngines.end())
        {
            found->second->focused.store(focused);
            if (owner) found->second->owner.store(owner, std::memory_order_release);
            found->second->metadataChanged.store(true);
        }
    }
    __declspec(guard(nocf)) void __fastcall hookedOwner(void* core, std::uint64_t owner)
    {
        originalOwner(core, owner);
        bool focused = false;
        {
            std::scoped_lock lock{ enginesMutex };
            if (const auto known = coreFocus.find(core); known != coreFocus.end())
                focused = known->second;
        }
        // Default-terminal handoff can create its ControlCore before injection
        // and assign the real window only later (TerminalPage::Initialize walks
        // the defterm panes and calls OwningHwnd). The setter is therefore an
        // authoritative lazy-recovery boundary just like _focusChanged. Without
        // this, that first delegated SSH tab has no native engine and hybrid mode
        // falls back to UIA; a subsequently created ordinary tab works by chance.
        recoverExistingCore(core, focused);

        std::scoped_lock lock{ enginesMutex };
        coreOwners[core] = owner;
        if (const auto found = coreEngines.find(core); found != coreEngines.end())
        {
            found->second->owner.store(owner);
            found->second->metadataChanged.store(true);
        }
    }
    __declspec(guard(nocf)) void __fastcall hookedAdd(void* renderer, RenderEngine* engine)
    {
        originalAdd(renderer, engine);
        if (!initializingCore) return;
        std::scoped_lock lock{ enginesMutex };
        if (coreEngines.contains(initializingCore)) return;
        auto capture = std::make_unique<CaptureEngine>(renderer);
        if (const auto owner = coreOwners.find(initializingCore); owner != coreOwners.end()) capture->owner.store(owner->second);
        if (const auto focus = coreFocus.find(initializingCore); focus != coreFocus.end()) capture->focused.store(focus->second);
        auto* pointer = capture.get();
        coreEngines.emplace(initializingCore, pointer);
        engines.push_back(std::move(capture));
        originalAdd(renderer, pointer);
        pointer->attached.store(true, std::memory_order_release);
    }

    bool installHooks(HMODULE module)
    {
        auto address = [&](std::uint32_t id) { return reinterpret_cast<std::uint8_t*>(module) + profileEntries.at(id).rva; };
        auto* add = address(8);
        // wt_1_24's AddRenderEngine starts with sub rsp,38 / test rdx,rdx /
        // je rel32. Its special trampoline preserves both paths exactly.
        const std::array<std::uint8_t, 9> addPrefix{ 0x48, 0x83, 0xec, 0x38, 0x48, 0x85, 0xd2, 0x0f, 0x84 };
        if (std::memcmp(add, addPrefix.data(), addPrefix.size()) != 0) return false;
        auto emitAbsoluteJump = [](std::uint8_t* output, std::size_t& offset, const std::uintptr_t destination) {
            output[offset++] = 0xff;
            output[offset++] = 0x25;
            std::memset(output + offset, 0, 4);
            offset += 4;
            std::memcpy(output + offset, &destination, sizeof(destination));
            offset += sizeof(destination);
        };
        auto* addTrampoline = static_cast<std::uint8_t*>(VirtualAlloc(nullptr, 64, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE));
        if (!addTrampoline) return false;
        std::size_t n = 0;
        std::memcpy(addTrampoline + n, add, 7); n += 7;
        addTrampoline[n++] = 0x75; addTrampoline[n++] = 14; // non-null -> copied LEA
        std::int32_t relative = 0; std::memcpy(&relative, add + 9, 4);
        emitAbsoluteJump(addTrampoline, n, reinterpret_cast<std::uintptr_t>(add + 13 + relative));
        std::memcpy(addTrampoline + n, add + 13, 4); n += 4; // lea rax,[rcx+8]
        emitAbsoluteJump(addTrampoline, n, reinterpret_cast<std::uintptr_t>(add + 17));
        DWORD old = 0;
        if (!VirtualProtect(addTrampoline, 64, PAGE_EXECUTE_READ, &old)) return false;
        originalAdd = reinterpret_cast<AddFn>(addTrampoline);

        auto* init = address(1);
        auto* destroy = address(2);
        auto* focus = address(3);
        auto* owner = address(4);
        originalInitialize = reinterpret_cast<InitializeFn>(absoluteTrampoline(init, 20));
        originalDestructor = reinterpret_cast<DestructorFn>(absoluteTrampoline(destroy, 15));
        originalFocus = reinterpret_cast<FocusFn>(absoluteTrampoline(focus, 15));

        // Rewrite the owner's RIP-relative security-cookie load to an absolute
        // load. VirtualAlloc may be more than 2 GiB from WT, so merely adjusting
        // its rel32 displacement is not representable (the isolated crash dump
        // proved that failure mode at the faulting trampoline instruction).
        const std::array<std::uint8_t, 3> cookieLoad{ 0x48, 0x8b, 0x05 };
        if (std::memcmp(owner + 10, cookieLoad.data(), cookieLoad.size()) != 0) return false;
        std::int32_t cookieRelative = 0;
        std::memcpy(&cookieRelative, owner + 13, sizeof(cookieRelative));
        const auto cookieAddress = reinterpret_cast<std::uintptr_t>(owner + 17 + cookieRelative);
        auto* ownerTrampoline = static_cast<std::uint8_t*>(VirtualAlloc(nullptr, 64, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE));
        if (!ownerTrampoline) return false;
        n = 0;
        std::memcpy(ownerTrampoline, owner, 10); n += 10;
        ownerTrampoline[n++] = 0x48; ownerTrampoline[n++] = 0xb8;
        std::memcpy(ownerTrampoline + n, &cookieAddress, sizeof(cookieAddress)); n += sizeof(cookieAddress);
        ownerTrampoline[n++] = 0x48; ownerTrampoline[n++] = 0x8b; ownerTrampoline[n++] = 0x00;
        emitAbsoluteJump(ownerTrampoline, n, reinterpret_cast<std::uintptr_t>(owner + 17));
        if (!VirtualProtect(ownerTrampoline, 64, PAGE_EXECUTE_READ, &old)) return false;
        originalOwner = reinterpret_cast<OwnerFn>(ownerTrampoline);

        triggerRedraw = reinterpret_cast<TriggerFn>(address(10));
        underlineColor = reinterpret_cast<UnderlineColorFn>(address(11));
        if (!originalInitialize || !originalDestructor || !originalFocus || !originalOwner) return false;
        return patchHook(add, reinterpret_cast<void*>(hookedAdd), 17) &&
               patchHook(init, reinterpret_cast<void*>(hookedInitialize), 20) &&
               patchHook(destroy, reinterpret_cast<void*>(hookedDestructor), 15) &&
               patchHook(focus, reinterpret_cast<void*>(hookedFocus), 15) &&
               patchHook(owner, reinterpret_cast<void*>(hookedOwner), 17);
    }

    std::string utf8(const wchar_t* data, std::size_t length)
    {
        if (!data || length == 0) return " ";
        const auto needed = WideCharToMultiByte(CP_UTF8, WC_ERR_INVALID_CHARS, data, static_cast<int>(length), nullptr, 0, nullptr, nullptr);
        if (needed <= 0) return "\xef\xbf\xbd";
        std::string out(static_cast<std::size_t>(needed), '\0');
        WideCharToMultiByte(CP_UTF8, WC_ERR_INVALID_CHARS, data, static_cast<int>(length), out.data(), needed, nullptr, nullptr);
        return out;
    }

    std::vector<std::uint8_t> encodePng(std::vector<Pixel>& pixels, const std::uint32_t width, const std::uint32_t height)
    {
        for (auto& pixel : pixels) pixel.alpha = 255;
        IWICImagingFactory* factory = nullptr;
        IWICBitmapEncoder* encoder = nullptr;
        IWICBitmapFrameEncode* frame = nullptr;
        IPropertyBag2* properties = nullptr;
        IStream* stream = nullptr;
        std::vector<std::uint8_t> result;
        const auto cleanup = [&] {
            if (properties) properties->Release();
            if (frame) frame->Release();
            if (encoder) encoder->Release();
            if (stream) stream->Release();
            if (factory) factory->Release();
        };
        if (FAILED(CoCreateInstance(CLSID_WICImagingFactory, nullptr, CLSCTX_INPROC_SERVER,
                                    IID_PPV_ARGS(&factory))) ||
            FAILED(CreateStreamOnHGlobal(nullptr, TRUE, &stream)) ||
            FAILED(factory->CreateEncoder(GUID_ContainerFormatPng, nullptr, &encoder)) ||
            FAILED(encoder->Initialize(stream, WICBitmapEncoderNoCache)) ||
            FAILED(encoder->CreateNewFrame(&frame, &properties)) ||
            FAILED(frame->Initialize(properties)) || FAILED(frame->SetSize(width, height)))
        {
            cleanup();
            return result;
        }
        auto format = GUID_WICPixelFormat32bppBGRA;
        const auto stride = static_cast<UINT>(width * sizeof(Pixel));
        if (FAILED(frame->SetPixelFormat(&format)) || format != GUID_WICPixelFormat32bppBGRA ||
            FAILED(frame->WritePixels(height, stride, stride * height,
                                      reinterpret_cast<BYTE*>(pixels.data()))) ||
            FAILED(frame->Commit()) || FAILED(encoder->Commit()))
        {
            cleanup();
            return result;
        }
        HGLOBAL memory = nullptr;
        if (SUCCEEDED(GetHGlobalFromStream(stream, &memory)) && memory)
        {
            const auto size = GlobalSize(memory);
            if (size && size <= 16 * 1024 * 1024)
            {
                if (const auto* bytes = static_cast<const std::uint8_t*>(GlobalLock(memory)))
                {
                    result.assign(bytes, bytes + size);
                    GlobalUnlock(memory);
                }
            }
        }
        cleanup();
        return result;
    }

    std::string imageKey(const std::vector<std::uint8_t>& png)
    {
        constexpr std::string_view mime{ "image/png" };
        std::vector<std::uint8_t> material;
        material.reserve(mime.size() + 1 + png.size());
        material.insert(material.end(), mime.begin(), mime.end());
        material.push_back(0);
        material.insert(material.end(), png.begin(), png.end());
        std::array<std::uint8_t, 32> digest{};
        if (!sha256(material, digest.data())) return {};
        constexpr char hex[]{ "0123456789abcdef" };
        std::string key(64, '0');
        for (std::size_t i = 0; i < digest.size(); ++i)
        {
            key[i * 2] = hex[digest[i] >> 4];
            key[i * 2 + 1] = hex[digest[i] & 15];
        }
        return key;
    }

    void processImages(CaptureEngine& engine, const Batch& batch)
    {
        std::vector<CaptureEngine::EncodedImage> painted;
        for (std::uint16_t i = 0; i < batch.imageCount;)
        {
            const auto& first = batch.images[i];
            auto end = static_cast<std::uint16_t>(i + 1);
            while (end < batch.imageCount)
            {
                const auto& next = batch.images[end];
                // Revisions are row-local in WT and need not match across slices
                // of one image. Adjacency plus identical raster/cell geometry is
                // the renderer's grouping contract; merging stacked independent
                // images with the same geometry is visually equivalent.
                if (next.targetRow != first.targetRow + (end - i) ||
                    next.cellSize.x != first.cellSize.x || next.cellSize.y != first.cellSize.y ||
                    next.columnBegin != first.columnBegin || next.columnEnd != first.columnEnd ||
                    next.pixelWidth != first.pixelWidth)
                    break;
                ++end;
            }
            const auto sliceCount = static_cast<std::uint32_t>(end - i);
            const auto height = static_cast<std::uint32_t>(first.cellSize.y) * sliceCount;
            const auto width = static_cast<std::uint32_t>(first.pixelWidth);
            const auto total = static_cast<std::uint64_t>(width) * height;
            const auto col = first.columnBegin - first.viewportLeft;
            if (total <= maxImagePixels && first.targetRow >= INT16_MIN && first.targetRow <= INT16_MAX &&
                col >= 0 && col <= UINT16_MAX)
            {
                std::vector<Pixel> pixels;
                pixels.reserve(static_cast<std::size_t>(total));
                for (auto index = i; index < end; ++index)
                {
                    const auto& image = batch.images[index];
                    const auto* begin = batch.imagePixels.data() + image.pixelOffset;
                    pixels.insert(pixels.end(), begin, begin + image.pixelCount);
                }
                auto png = encodePng(pixels, width, height);
                auto key = imageKey(png);
                if (!png.empty() && !key.empty())
                {
                    painted.push_back({ static_cast<std::int16_t>(first.targetRow),
                                        static_cast<std::uint16_t>(col),
                                        static_cast<float>(width) / first.cellSize.x,
                                        static_cast<float>(height) / first.cellSize.y,
                                        std::move(key), std::move(png) });
                }
            }
            i = end;
        }

        // PaintImageSlice is dirty-region based: cursor/text-only paints do not
        // replay unchanged images. Preserve placements outside the dirty area,
        // remove only placements whose cells were repainted, then install the
        // slices observed in this presentation. A full repaint with no slices
        // correctly clears all images.
        if (batch.fullRepaint)
        {
            engine.modelImages.clear();
        }
        else
        {
            const auto& dirty = batch.dirtyArea;
            std::erase_if(engine.modelImages, [&](const CaptureEngine::EncodedImage& image) {
                const auto imageRight = static_cast<float>(image.col) + image.cols;
                const auto imageBottom = static_cast<float>(image.row) + image.rows;
                return static_cast<float>(image.col) < dirty.right && imageRight > dirty.left &&
                       static_cast<float>(image.row) < dirty.bottom && imageBottom > dirty.top;
            });
        }
        engine.modelImages.insert(engine.modelImages.end(),
                                  std::make_move_iterator(painted.begin()),
                                  std::make_move_iterator(painted.end()));
    }

    void applyBatch(CaptureEngine& engine, const Batch& batch)
    {
        if (batch.rows == 0 || batch.cols == 0) return;
        if (engine.modelRows != batch.rows || engine.modelCols != batch.cols)
        {
            engine.modelRows = batch.rows;
            engine.modelCols = batch.cols;
            engine.matrix.assign(static_cast<std::size_t>(batch.rows) * batch.cols, {});
        }
        for (std::uint32_t i = 0; i < batch.lineCount; ++i)
        {
            const auto& line = batch.lines[i];
            auto column = static_cast<std::uint32_t>(line.firstColumn);
            auto textOffset = line.firstText;
            for (std::uint32_t j = 0; j < line.clusterCount; ++j)
            {
                const auto& event = batch.clusters[line.firstCluster + j];
                if (event.styleIndex >= batch.styleCount || textOffset + event.textLength > batch.textLength) break;
                if (line.row < engine.modelRows && column < engine.modelCols)
                {
                    auto& cell = engine.matrix[static_cast<std::size_t>(line.row) * engine.modelCols + column];
                    cell.text = utf8(batch.text.data() + textOffset, event.textLength);
                    cell.width = event.width;
                    cell.continuation = false;
                    cell.style = batch.styles[event.styleIndex];
                    if (event.width == 2 && column + 1 < engine.modelCols)
                    {
                        auto& continuation = engine.matrix[static_cast<std::size_t>(line.row) * engine.modelCols + column + 1];
                        continuation = {};
                        continuation.continuation = true;
                    }
                }
                textOffset += event.textLength;
                column += event.width;
            }
        }
        const auto applyHighlight = [&](const PointSpan& span, const COLORREF background) {
            for (auto absoluteRow = span.start.y; absoluteRow <= span.end.y; ++absoluteRow)
            {
                const auto row = absoluteRow - batch.viewportTop;
                if (row < 0 || row >= static_cast<int>(engine.modelRows)) continue;
                const auto first = absoluteRow == span.start.y ? span.start.x : batch.viewportLeft;
                const auto last = absoluteRow == span.end.y ? span.end.x :
                                  batch.viewportLeft + static_cast<std::int32_t>(engine.modelCols) - 1;
                const auto left = (std::clamp)(first - batch.viewportLeft, 0, static_cast<int>(engine.modelCols) - 1);
                const auto right = (std::clamp)(last - batch.viewportLeft, 0, static_cast<int>(engine.modelCols) - 1);
                for (auto col = left; col <= right; ++col)
                {
                    auto& cell = engine.matrix[static_cast<std::size_t>(row) * engine.modelCols + col];
                    if (!cell.continuation)
                    {
                        cell.style.foreground = RGB(0, 0, 0);
                        cell.style.background = background;
                    }
                }
            }
        };
        for (std::uint16_t i = 0; i < batch.searchCount; ++i)
            applyHighlight(batch.searchHighlights[i], RGB(255, 255, 0));
        if (batch.hasFocusedSearch) applyHighlight(batch.focusedSearch, RGB(255, 150, 50));

        for (std::uint16_t i = 0; i < batch.selectionCount; ++i)
        {
            const auto& rect = batch.selections[i];
            const auto top = (std::clamp)(rect.top, 0, static_cast<int>(engine.modelRows));
            const auto bottom = (std::clamp)(rect.bottom, 0, static_cast<int>(engine.modelRows));
            const auto left = (std::clamp)(rect.left, 0, static_cast<int>(engine.modelCols));
            const auto right = (std::clamp)(rect.right, 0, static_cast<int>(engine.modelCols));
            for (auto row = top; row < bottom; ++row)
            {
                for (auto col = left; col < right; ++col)
                {
                    auto& cell = engine.matrix[static_cast<std::size_t>(row) * engine.modelCols + col];
                    if (!cell.continuation) cell.style.background = batch.selectionBackground;
                }
            }
        }
        if (batch.titleLength) engine.modelTitle = utf8(batch.title.data(), batch.titleLength);
        engine.modelDefaultForeground = batch.defaultForeground;
        engine.modelDefaultBackground = batch.defaultBackground;
        engine.cursorVisible = batch.hasCursor && batch.cursor.visible && batch.cursor.inViewport;
        if (engine.cursorVisible)
        {
            engine.cursorRow = static_cast<std::uint16_t>((std::max)(batch.cursor.position.y, 0));
            engine.cursorCol = static_cast<std::uint16_t>((std::max)(batch.cursor.position.x, 0));
            engine.cursorStyle = batch.cursor.type == 1 ? 6 : (batch.cursor.type == 2 || batch.cursor.type == 5 ? 4 : 2);
        }
    }

    __declspec(guard(nocf)) std::vector<std::uint8_t> framePayload(CaptureEngine& engine)
    {
        std::vector<Style> styles;
        styles.reserve(64);
        auto styleIndex = [&](const Style& style) {
            const auto found = std::find(styles.begin(), styles.end(), style);
            if (found != styles.end()) return static_cast<std::uint16_t>(found - styles.begin());
            if (styles.size() >= 4096) return std::uint16_t{ 0 };
            styles.push_back(style);
            return static_cast<std::uint16_t>(styles.size() - 1);
        };
        for (const auto& cell : engine.matrix) if (!cell.continuation) styleIndex(cell.style);
        if (styles.empty()) styles.push_back({});

        std::unordered_map<std::uint16_t, std::string> links;
        {
            // ControlCore destruction takes this registry mutex before it marks
            // the engine dead. Hold it only around borrowed RenderData access;
            // release it before any pipe write so a stalled broker can never
            // block target focus/lifecycle hooks.
            std::scoped_lock registry{ enginesMutex };
            if (auto* data = engine.renderData.load(std::memory_order_acquire); data && engine.alive.load())
            {
                data->LockConsole();
                try
                {
                    for (const auto& style : styles)
                    {
                        if (style.hyperlink == 0 || links.contains(style.hyperlink)) continue;
                        const auto uri = data->GetHyperlinkUri(style.hyperlink);
                        auto encoded = uri.empty() ? std::string{} : utf8(uri.data(), uri.size());
                        if (!encoded.empty() && encoded.size() <= 8192) links.emplace(style.hyperlink, std::move(encoded));
                    }
                }
                catch (...)
                {
                    links.clear();
                }
                data->UnlockConsole();
            }
        }

        std::vector<std::uint8_t> out;
        out.reserve(engine.matrix.size() * 12);
        append(out, engine.source);
        append(out, std::uint64_t{ 1 });
        append(out, engine.frameSequence++);
        append(out, engine.modelRows);
        append(out, engine.modelCols);
        appendColor(out, engine.modelDefaultForeground);
        appendColor(out, engine.modelDefaultBackground);
        append(out, static_cast<std::uint16_t>(styles.size()));
        for (const auto& style : styles)
        {
            appendColor(out, style.foreground);
            appendColor(out, style.background);
            append(out, style.flags);
            out.push_back(style.underline);
            appendColor(out, style.underlineColor);
            append(out, style.hyperlink != 0 && links.contains(style.hyperlink) ?
                            static_cast<std::uint32_t>(style.hyperlink) : UINT32_MAX);
        }
        append(out, static_cast<std::uint16_t>(links.size()));
        for (const auto& [id, uri] : links)
        {
            append(out, static_cast<std::uint32_t>(id));
            appendString(out, uri);
        }
        for (std::uint16_t row = 0; row < engine.modelRows; ++row)
        {
            std::uint16_t count = 0;
            for (std::uint16_t col = 0; col < engine.modelCols; ++col)
                if (!engine.matrix[static_cast<std::size_t>(row) * engine.modelCols + col].continuation) ++count;
            append(out, count);
            for (std::uint16_t col = 0; col < engine.modelCols; ++col)
            {
                const auto& cell = engine.matrix[static_cast<std::size_t>(row) * engine.modelCols + col];
                if (cell.continuation) continue;
                append(out, col);
                out.push_back(cell.width);
                append(out, styleIndex(cell.style));
                appendString(out, cell.text);
            }
        }
        out.push_back(engine.cursorVisible ? 1 : 0);
        if (engine.cursorVisible)
        {
            append(out, engine.cursorRow);
            append(out, engine.cursorCol);
        }
        out.push_back(engine.cursorStyle);
        appendString(out, engine.modelTitle);
        append(out, static_cast<std::uint16_t>(engine.modelImages.size()));
        for (const auto& image : engine.modelImages)
        {
            append(out, image.row);
            append(out, image.col);
            append(out, std::bit_cast<std::uint32_t>(image.cols));
            append(out, std::bit_cast<std::uint32_t>(image.rows));
            out.insert(out.end(), image.key.begin(), image.key.end());
        }
        return out;
    }

    std::vector<std::uint8_t> helloPayload()
    {
        std::vector<std::uint8_t> out;
        out.push_back(1);
        out.push_back(1);
        append(out, GetCurrentProcessId());
        append(out, std::uint32_t{ 0 });
        appendString(out, std::string_view{ profile.family, strnlen_s(profile.family, sizeof(profile.family)) });
        out.push_back(1);
        out.insert(out.end(), moduleHash.begin(), moduleHash.end());
        return out;
    }
    std::vector<std::uint8_t> imageBlobPayload(const CaptureEngine& engine, const CaptureEngine::EncodedImage& image)
    {
        std::vector<std::uint8_t> out;
        append(out, engine.source);
        append(out, std::uint64_t{ 1 });
        out.insert(out.end(), image.key.begin(), image.key.end());
        appendString(out, "image/png");
        append(out, static_cast<std::uint32_t>(image.png.size()));
        out.insert(out.end(), image.png.begin(), image.png.end());
        return out;
    }
    std::vector<std::uint8_t> sourcePayload(const CaptureEngine& engine)
    {
        std::vector<std::uint8_t> out;
        append(out, engine.source);
        append(out, std::uint64_t{ 1 });
        append(out, engine.owner.load());
        append(out, engine.rows.load());
        append(out, engine.cols.load());
        std::uint32_t flags = 2;
        if (engine.focused.load()) flags |= 1;
        append(out, flags);
        appendString(out, engine.modelTitle.empty() ? "Windows Terminal" : engine.modelTitle);
        return out;
    }
    std::vector<std::uint8_t> updatedPayload(const CaptureEngine& engine)
    {
        std::vector<std::uint8_t> out;
        append(out, engine.source);
        append(out, std::uint64_t{ 1 });
        out.push_back(0x1f); // owner, dimensions, focused, visible, title
        append(out, engine.owner.load());
        append(out, engine.rows.load());
        append(out, engine.cols.load());
        out.push_back(engine.focused.load() ? 1 : 0);
        out.push_back(1);
        appendString(out, engine.modelTitle.empty() ? "Windows Terminal" : engine.modelTitle);
        return out;
    }
    struct PerformanceSnapshot
    {
        std::uint64_t count = 0;
        std::array<std::uint64_t, 5> buckets{};
    };

    std::vector<std::uint8_t> performancePayload(const CaptureEngine& engine, PerformanceSnapshot& snapshot)
    {
        snapshot.count = engine.performanceCount.load(std::memory_order_acquire);
        std::array<std::uint64_t, 5> samples{};
        std::uint64_t sampleCount = 0;
        for (std::size_t i = 0; i < samples.size(); ++i)
        {
            snapshot.buckets[i] = engine.performanceBuckets[i].load(std::memory_order_relaxed);
            samples[i] = snapshot.buckets[i] - engine.performanceReportedBuckets[i];
            sampleCount += samples[i];
        }
        const auto target = (sampleCount * 95 + 99) / 100;
        std::uint64_t cumulative = 0;
        constexpr std::array<std::uint64_t, 5> ceilings{ 250, 500, 1000, 2000, 999999 };
        std::uint64_t p95 = ceilings.back();
        for (std::size_t i = 0; i < ceilings.size(); ++i)
        {
            cumulative += samples[i];
            if (cumulative >= target)
            {
                p95 = ceilings[i];
                break;
            }
        }
        const auto text = std::string{ "WT render callback p95<=" } + std::to_string(p95) +
                          "us max=" + std::to_string(engine.performanceMaxMicros.load(std::memory_order_relaxed)) +
                          "us count=" + std::to_string(snapshot.count) +
                          " sample=" + std::to_string(sampleCount) +
                          " dropped=" + std::to_string(engine.droppedFrames.load(std::memory_order_relaxed)) +
                          " layouts=" + std::to_string(engine.layoutChanges.load(std::memory_order_relaxed)) +
                          " stale_layout=" + std::to_string(engine.staleLayoutBatches.load(std::memory_order_relaxed));
        std::vector<std::uint8_t> out;
        out.push_back(1);
        append(out, engine.source);
        append(out, std::uint16_t{ 201 });
        appendString(out, text);
        return out;
    }
    std::vector<std::uint8_t> faultPayload(const CaptureEngine& engine)
    {
        std::vector<std::uint8_t> out;
        out.push_back(1);
        append(out, engine.source);
        append(out, std::uint16_t{ 202 });
        appendString(out, "WT capture provider disabled after an internal callback fault");
        return out;
    }
    std::vector<std::uint8_t> removedPayload(const CaptureEngine& engine)
    {
        std::vector<std::uint8_t> out;
        append(out, engine.source);
        append(out, std::uint64_t{ 1 });
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
        const auto pipe = CreateFileW(name.c_str(), GENERIC_READ | GENERIC_WRITE, 0, nullptr, OPEN_EXISTING, 0, nullptr);
        return pipe;
    }
    std::uint16_t le16(const std::uint8_t* p) { return static_cast<std::uint16_t>(p[0] | (p[1] << 8)); }
    std::uint32_t le32(const std::uint8_t* p) { return static_cast<std::uint32_t>(p[0] | (p[1] << 8) | (p[2] << 16) | (p[3] << 24)); }
    std::uint64_t le64(const std::uint8_t* p)
    {
        std::uint64_t value = 0;
        for (std::size_t i = 0; i < 8; ++i) value |= static_cast<std::uint64_t>(p[i]) << (8 * i);
        return value;
    }
    bool readExact(HANDLE pipe, void* output, std::size_t length)
    {
        auto* bytes = static_cast<std::uint8_t*>(output);
        while (length)
        {
            DWORD count = 0;
            if (!ReadFile(pipe, bytes, static_cast<DWORD>(length), &count, nullptr) || count == 0) return false;
            bytes += count;
            length -= count;
        }
        return true;
    }

    DWORD WINAPI worker(void*)
    {
        if (FAILED(CoInitializeEx(nullptr, COINIT_MULTITHREADED))) return 1;
        auto* control = GetModuleHandleW(L"Microsoft.Terminal.Control.dll");
        if (!control || !loadProfile() || !verifyModule(control) || !installHooks(control)) return 1;
        const auto nonce = (static_cast<std::uint64_t>(GetCurrentProcessId()) << 32) ^ GetTickCount64();
        std::vector<CaptureEngine*> knownEngines;
        const auto refreshEngines = [&] {
            std::scoped_lock lock{ enginesMutex };
            knownEngines.clear();
            knownEngines.reserve(engines.size());
            for (auto& engine : engines) knownEngines.push_back(engine.get());
        };
        while (true)
        {
            const auto pipe = connectBroker();
            if (pipe == INVALID_HANDLE_VALUE)
            {
                refreshEngines();
                for (auto* engine : knownEngines) engine->reclaimIfDormant();
                Sleep(250);
                continue;
            }
            std::uint64_t sequence = 1;
            std::unordered_set<std::string> sentImages;
            if (!sendPacket(pipe, MessageType::hello, nonce, sequence++, helloPayload()))
            {
                CloseHandle(pipe);
                refreshEngines();
                for (auto* engine : knownEngines)
                {
                    engine->park();
                    engine->reclaimIfDormant();
                }
                continue;
            }
            refreshEngines();
            for (auto* engine : knownEngines) engine->sourceSent = false;
            bool connected = true;
            while (connected)
            {
                refreshEngines();
                for (auto* engine : knownEngines)
                {
                        if (!engine->attached.load(std::memory_order_acquire)) continue;
                        engine->reclaimIfDormant();
                        if (!engine->alive.load())
                        {
                            // Removal precedes the fault diagnostic on this FIFO.
                            // Observing code 202 therefore also proves that the
                            // broker was told to retire the failed provider.
                            if (engine->sourceSent)
                            {
                                connected = sendPacket(pipe, MessageType::sourceRemoved, nonce, sequence++, removedPayload(*engine));
                                engine->sourceSent = false;
                                if (!connected) break;
                            }
                            if (engine->faulted.load() && !engine->faultDiagnosticSent)
                            {
                                connected = sendPacket(pipe, MessageType::diagnostic, nonce, sequence++, faultPayload(*engine));
                                engine->faultDiagnosticSent = connected;
                                if (!connected) break;
                            }
                            continue;
                        }
                        const auto performanceCount = engine->performanceCount.load(std::memory_order_relaxed);
                        if (performanceCount >= engine->performanceReportedCount + 120)
                        {
                            PerformanceSnapshot snapshot;
                            auto payload = performancePayload(*engine, snapshot);
                            connected = sendPacket(pipe, MessageType::diagnostic, nonce, sequence++, payload);
                            if (connected)
                            {
                                engine->performanceReportedCount = snapshot.count;
                                engine->performanceReportedBuckets = snapshot.buckets;
                            }
                            if (!connected) break;
                        }
                        if (!engine->sourceSent && engine->rows.load() && engine->cols.load())
                        {
                            connected = sendPacket(pipe, MessageType::sourceAdded, nonce, sequence++, sourcePayload(*engine));
                            engine->sourceSent = connected;
                        }
                        else if (engine->sourceSent && engine->metadataChanged.exchange(false))
                        {
                            connected = sendPacket(pipe, MessageType::sourceUpdated, nonce, sequence++, updatedPayload(*engine));
                        }
                        if (auto* batch = engine->takeBatch())
                        {
                            const auto viewportMatches = [&] {
                                const auto version = engine->viewportVersion.load(std::memory_order_acquire);
                                return (version & 1) == 0 && batch->viewportVersion == version &&
                                       batch->rows == engine->rows.load(std::memory_order_acquire) &&
                                       batch->cols == engine->cols.load(std::memory_order_acquire);
                            };
                            if (viewportMatches())
                            {
                                applyBatch(*engine, *batch);
                                // The worker may have taken a completed old batch
                                // just before a rapid resize moved away and back to
                                // the same cell dimensions. Recheck the generation
                                // after model mutation; dimensions alone cannot
                                // distinguish that stale presentation.
                                if (viewportMatches()) processImages(*engine, *batch);
                                if (viewportMatches())
                                {
                                    if (engine->sourceSent)
                                    {
                                        for (const auto& image : engine->modelImages)
                                        {
                                            // IMAGE_BLOB is source/generation scoped by
                                            // the native protocol. Identical content in
                                            // two panes therefore must be announced once
                                            // for each source, not once per connection.
                                            const auto sentKey = std::to_string(engine->source) + ':' + image.key;
                                            if (sentImages.insert(sentKey).second)
                                                connected = sendPacket(pipe, MessageType::imageBlob, nonce, sequence++, imageBlobPayload(*engine, image));
                                            if (!connected) break;
                                        }
                                        if (connected) connected = sendPacket(pipe, MessageType::frame, nonce, sequence++, framePayload(*engine));
                                    }
                                }
                            }
                            else
                            {
                                engine->staleLayoutBatches.fetch_add(1, std::memory_order_relaxed);
                            }
                            // UpdateViewport itself invalidates and wakes WT's
                            // renderer. A mismatched queued batch is therefore
                            // only discarded here; requesting another synthetic
                            // full can race a sixel presentation and clear an
                            // otherwise-current image placement.
                            engine->releaseBatch(batch);
                        }
                        if (!connected) break;
                }
                if (!connected) break;

                DWORD available = 0;
                std::array<std::uint8_t, 28> envelope{};
                DWORD peeked = 0;
                if (!PeekNamedPipe(pipe, envelope.data(), static_cast<DWORD>(envelope.size()), &peeked, &available, nullptr)) break;
                if (available >= envelope.size() && peeked == envelope.size())
                {
                    const auto length = le32(envelope.data() + 8);
                    if (length > 1024 * 1024 || available < envelope.size() + length)
                    {
                        if (length > 1024 * 1024) break;
                    }
                    else
                    {
                        if (!readExact(pipe, envelope.data(), envelope.size()) ||
                            !std::equal(wireMagic.begin(), wireMagic.end(), envelope.begin()) ||
                            le16(envelope.data() + 4) != wireVersion) break;
                        std::vector<std::uint8_t> payload(length);
                        if (!readExact(pipe, payload.data(), payload.size())) break;
                        const auto type = static_cast<MessageType>(le16(envelope.data() + 6));
                        if ((type == MessageType::subscribe || type == MessageType::unsubscribe || type == MessageType::requestFull) && payload.size() >= 16)
                        {
                            const auto source = le64(payload.data());
                            const auto generation = le64(payload.data() + 8);
                            if (generation == 1)
                            {
                                // requestFull calls the borrowed Renderer while
                                // ControlCore destruction uses the same mutex to
                                // retire it. No IPC occurs in this critical section.
                                std::scoped_lock registry{ enginesMutex };
                                for (auto* engine : knownEngines)
                                {
                                    if (engine->source != source) continue;
                                    if (type == MessageType::unsubscribe) engine->park();
                                    else engine->requestFull();
                                }
                            }
                        }
                        else if (type == MessageType::shutdown)
                        {
                            CloseHandle(pipe);
                            refreshEngines();
                            for (auto* engine : knownEngines)
                            {
                                engine->park();
                                engine->reclaimIfDormant();
                            }
                            return 0;
                        }
                    }
                }
                Sleep(2);
            }
            CloseHandle(pipe);
            refreshEngines();
            for (auto* engine : knownEngines)
            {
                engine->park();
                engine->reclaimIfDormant();
                engine->sourceSent = false;
            }
            Sleep(250);
        }
    }
}

BOOL WINAPI DllMain(HINSTANCE instance, DWORD reason, void*)
{
    if (reason == DLL_PROCESS_ATTACH)
    {
        selfModule = instance;
        DisableThreadLibraryCalls(instance);
        if (const auto thread = CreateThread(nullptr, 0, worker, nullptr, 0, nullptr)) CloseHandle(thread);
    }
    return TRUE;
}
