// Generate a fail-closed ABI profile for one exact Windows Terminal/conhost
// module. DIA loads the PDB identified by the PE (honoring _NT_SYMBOL_PATH),
// resolves exact undecorated function signatures, and records target identity,
// executable-section RVAs, and prologue bytes. The injected adapter accepts only
// profiles whose complete identity still matches.

#include <windows.h>
#include <bcrypt.h>
#include <dia2.h>

#include <algorithm>
#include <array>
#include <cstdint>
#include <cstdio>
#include <climits>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <optional>
#include <span>
#include <string>
#include <string_view>
#include <vector>

#pragma comment(lib, "bcrypt.lib")

namespace
{
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

    struct SymbolSpec
    {
        std::uint32_t id;
        const char* label;
        std::vector<std::string_view> required;
        std::vector<std::string_view> forbidden;
        std::uint32_t parameterCount;
    };

    // IDs are frozen: compiled ABI shims consume these, profile files only carry
    // addresses/identity and cannot redefine what a hook means.
    const std::array wtSpecs{
        SymbolSpec{ 1, "ControlCore::Initialize(float,float,float)", { "implementation::ControlCore::Initialize" }, {}, 3 },
        SymbolSpec{ 2, "ControlCore::~ControlCore", { "implementation::ControlCore::~ControlCore" }, { "scalar deleting", "vector deleting" }, 0 },
        // This ABI family uses the documented fallback when optimized builds
        // inline GotFocus/LostFocus: one exact _focusChanged(bool) hook.
        SymbolSpec{ 3, "ControlCore::_focusChanged(bool)", { "implementation::ControlCore::_focusChanged" }, {}, 1 },
        SymbolSpec{ 4, "ControlCore::OwningHwnd(uint64_t)", { "implementation::ControlCore::OwningHwnd" }, {}, 1 },
        SymbolSpec{ 8, "Renderer::AddRenderEngine", { "Renderer::AddRenderEngine" }, {}, 1 },
        // Optimized stock WT inlines/removes RemoveRenderEngine. The adapter DLL
        // and its engines are process-lifetime objects; core destruction marks the
        // source dead before the renderer drops its pointer vector, so no detach
        // call is needed and requiring that eliminated symbol rejects valid builds.
        SymbolSpec{ 10, "Renderer::TriggerRedrawAll", { "Renderer::TriggerRedrawAll" }, {}, 2 },
        SymbolSpec{ 11, "RenderSettings::GetAttributeUnderlineColor", { "RenderSettings::GetAttributeUnderlineColor" }, {}, 1 },
    };

    // Shipped conhost public PDBs omit function type records (`<no type>`), so
    // this adapter family pins the exact signature separately and the profile
    // records the symbol-verified address/prologue. The family shim is selected
    // only for a known module hash.
    const std::array conhostSpecs{
        SymbolSpec{ 100, "Renderer::AddRenderEngine", { "Renderer::AddRenderEngine" }, {}, UINT32_MAX },
        SymbolSpec{ 101, "Renderer::TriggerRedrawAll", { "Renderer::TriggerRedrawAll" }, {}, UINT32_MAX },
        SymbolSpec{ 102, "Renderer::PaintFrame", { "Renderer::PaintFrame" }, {}, UINT32_MAX },
    };

    struct Pe
    {
        std::vector<std::uint8_t> bytes;
        const IMAGE_NT_HEADERS64* nt = nullptr;
        std::span<const IMAGE_SECTION_HEADER> sections;

        const std::uint8_t* rva(const std::uint32_t value, const std::size_t length) const
        {
            for (const auto& section : sections)
            {
                const auto extent = (std::max)(section.Misc.VirtualSize, section.SizeOfRawData);
                if (value >= section.VirtualAddress && value - section.VirtualAddress <= extent &&
                    length <= extent - (value - section.VirtualAddress))
                {
                    const auto offset = static_cast<std::size_t>(section.PointerToRawData) + value - section.VirtualAddress;
                    if (offset <= bytes.size() && length <= bytes.size() - offset)
                    {
                        return bytes.data() + offset;
                    }
                }
            }
            return nullptr;
        }

        bool executable(const std::uint32_t value, const std::size_t length) const
        {
            for (const auto& section : sections)
            {
                if ((section.Characteristics & IMAGE_SCN_MEM_EXECUTE) == 0)
                {
                    continue;
                }
                const auto extent = (std::max)(section.Misc.VirtualSize, section.SizeOfRawData);
                if (value >= section.VirtualAddress && value - section.VirtualAddress <= extent &&
                    length <= extent - (value - section.VirtualAddress))
                {
                    return true;
                }
            }
            return false;
        }
    };

    std::optional<Pe> loadPe(const std::filesystem::path& path)
    {
        std::ifstream stream{ path, std::ios::binary | std::ios::ate };
        if (!stream)
        {
            return std::nullopt;
        }
        const auto size = stream.tellg();
        if (size <= 0 || size > 1024ll * 1024 * 1024)
        {
            return std::nullopt;
        }
        Pe pe;
        pe.bytes.resize(static_cast<std::size_t>(size));
        stream.seekg(0);
        stream.read(reinterpret_cast<char*>(pe.bytes.data()), size);
        if (!stream || pe.bytes.size() < sizeof(IMAGE_DOS_HEADER))
        {
            return std::nullopt;
        }
        const auto* dos = reinterpret_cast<const IMAGE_DOS_HEADER*>(pe.bytes.data());
        if (dos->e_magic != IMAGE_DOS_SIGNATURE || dos->e_lfanew < 0 ||
            static_cast<std::size_t>(dos->e_lfanew) + sizeof(IMAGE_NT_HEADERS64) > pe.bytes.size())
        {
            return std::nullopt;
        }
        pe.nt = reinterpret_cast<const IMAGE_NT_HEADERS64*>(pe.bytes.data() + dos->e_lfanew);
        if (pe.nt->Signature != IMAGE_NT_SIGNATURE ||
            pe.nt->OptionalHeader.Magic != IMAGE_NT_OPTIONAL_HDR64_MAGIC)
        {
            return std::nullopt;
        }
        const auto* first = IMAGE_FIRST_SECTION(pe.nt);
        const auto sectionOffset = static_cast<std::size_t>(
            reinterpret_cast<const std::uint8_t*>(first) - pe.bytes.data());
        if (sectionOffset > pe.bytes.size() ||
            static_cast<std::size_t>(pe.nt->FileHeader.NumberOfSections) >
                (pe.bytes.size() - sectionOffset) / sizeof(*first))
        {
            return std::nullopt;
        }
        pe.sections = { first, pe.nt->FileHeader.NumberOfSections };
        return pe;
    }

    bool sha256(const std::span<const std::uint8_t> bytes, std::uint8_t* output)
    {
        BCRYPT_ALG_HANDLE algorithm = nullptr;
        BCRYPT_HASH_HANDLE hash = nullptr;
        DWORD objectLength = 0;
        DWORD got = 0;
        if (BCryptOpenAlgorithmProvider(&algorithm, BCRYPT_SHA256_ALGORITHM, nullptr, 0) < 0 ||
            BCryptGetProperty(algorithm, BCRYPT_OBJECT_LENGTH,
                              reinterpret_cast<PUCHAR>(&objectLength), sizeof(objectLength), &got, 0) < 0)
        {
            return false;
        }
        std::vector<std::uint8_t> object(objectLength);
        const auto ok = BCryptCreateHash(algorithm, &hash, object.data(), objectLength, nullptr, 0, 0) >= 0 &&
                        BCryptHashData(hash, const_cast<PUCHAR>(bytes.data()), static_cast<ULONG>(bytes.size()), 0) >= 0 &&
                        BCryptFinishHash(hash, output, 32, 0) >= 0;
        if (hash)
        {
            BCryptDestroyHash(hash);
        }
        BCryptCloseAlgorithmProvider(algorithm, 0);
        return ok;
    }

    bool pdbIdentity(const Pe& pe, GUID& guid, std::uint32_t& age)
    {
        const auto& dir = pe.nt->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_DEBUG];
        const auto* debug = reinterpret_cast<const IMAGE_DEBUG_DIRECTORY*>(pe.rva(dir.VirtualAddress, dir.Size));
        if (!debug)
        {
            return false;
        }
        for (std::size_t i = 0; i < dir.Size / sizeof(*debug); ++i)
        {
            if (debug[i].Type != IMAGE_DEBUG_TYPE_CODEVIEW || debug[i].SizeOfData < 24)
            {
                continue;
            }
            const auto* cv = pe.rva(debug[i].AddressOfRawData, debug[i].SizeOfData);
            if (cv && std::memcmp(cv, "RSDS", 4) == 0)
            {
                std::memcpy(&guid, cv + 4, sizeof(guid));
                std::memcpy(&age, cv + 20, sizeof(age));
                return true;
            }
        }
        return false;
    }

    struct Candidate
    {
        std::string name;
        std::string decorated;
        std::uint32_t rva;
        std::uint32_t parameterCount;
    };

    std::string utf8(const BSTR value)
    {
        if (!value)
        {
            return {};
        }
        const auto length = static_cast<int>(SysStringLen(value));
        const auto needed = WideCharToMultiByte(CP_UTF8, WC_ERR_INVALID_CHARS, value, length, nullptr, 0, nullptr, nullptr);
        if (needed <= 0)
        {
            return {};
        }
        std::string result(static_cast<std::size_t>(needed), '\0');
        WideCharToMultiByte(CP_UTF8, WC_ERR_INVALID_CHARS, value, length, result.data(), needed, nullptr, nullptr);
        return result;
    }

    bool udt(IDiaSymbol* global, const wchar_t* name, const ULONGLONG expectedLength, IDiaSymbol** result)
    {
        IDiaEnumSymbols* matches = nullptr;
        if (FAILED(global->findChildren(SymTagUDT, name, nsCaseSensitive, &matches)) || !matches) return false;
        IDiaSymbol* symbol = nullptr;
        ULONG fetched = 0;
        bool found = false;
        while (matches->Next(1, &symbol, &fetched) == S_OK && fetched == 1)
        {
            ULONGLONG length = 0;
            if (SUCCEEDED(symbol->get_length(&length)) && length == expectedLength && !found)
            {
                *result = symbol;
                found = true;
                symbol = nullptr;
            }
            if (symbol) symbol->Release();
            symbol = nullptr;
        }
        matches->Release();
        return found;
    }

    bool memberOffset(IDiaSymbol* type, const wchar_t* name, const LONG expected)
    {
        IDiaEnumSymbols* matches = nullptr;
        if (FAILED(type->findChildren(SymTagData, name, nsCaseSensitive, &matches)) || !matches) return false;
        IDiaSymbol* symbol = nullptr;
        ULONG fetched = 0;
        LONG offset = LONG_MIN;
        const auto ok = matches->Next(1, &symbol, &fetched) == S_OK && fetched == 1 &&
                        SUCCEEDED(symbol->get_offset(&offset)) && offset == expected;
        if (symbol) symbol->Release();
        matches->Release();
        return ok;
    }

    bool memberOffsetAndLength(IDiaSymbol* type, const wchar_t* name, const LONG expectedOffset,
                               const ULONGLONG expectedLength)
    {
        IDiaEnumSymbols* matches = nullptr;
        if (FAILED(type->findChildren(SymTagData, name, nsCaseSensitive, &matches)) || !matches) return false;
        IDiaSymbol* symbol = nullptr;
        IDiaSymbol* memberType = nullptr;
        ULONG fetched = 0;
        LONG offset = LONG_MIN;
        ULONGLONG length = 0;
        const auto ok = matches->Next(1, &symbol, &fetched) == S_OK && fetched == 1 &&
                        SUCCEEDED(symbol->get_offset(&offset)) && offset == expectedOffset &&
                        SUCCEEDED(symbol->get_type(&memberType)) && memberType &&
                        SUCCEEDED(memberType->get_length(&length)) && length == expectedLength;
        if (memberType) memberType->Release();
        if (symbol) symbol->Release();
        matches->Release();
        return ok;
    }

    bool virtualOffset(IDiaSymbol* type, const wchar_t* name, const DWORD expected)
    {
        IDiaEnumSymbols* matches = nullptr;
        if (FAILED(type->findChildren(SymTagFunction, name, nsCaseSensitive, &matches)) || !matches) return false;
        IDiaSymbol* symbol = nullptr;
        ULONG fetched = 0;
        DWORD offset = UINT32_MAX;
        BOOL isVirtual = FALSE;
        const auto ok = matches->Next(1, &symbol, &fetched) == S_OK && fetched == 1 &&
                        SUCCEEDED(symbol->get_virtual(&isVirtual)) && isVirtual &&
                        SUCCEEDED(symbol->get_virtualBaseOffset(&offset)) && offset == expected;
        if (symbol) symbol->Release();
        matches->Release();
        return ok;
    }

    bool validateWt124Abi(IDiaSymbol* global, std::string& error)
    {
        IDiaSymbol* cluster = nullptr;
        IDiaSymbol* cursor = nullptr;
        IDiaSymbol* attribute = nullptr;
        IDiaSymbol* frameInfo = nullptr;
        IDiaSymbol* imageSlice = nullptr;
        IDiaSymbol* renderData = nullptr;
        IDiaSymbol* engine = nullptr;
        IDiaSymbol* renderer = nullptr;
        IDiaSymbol* controlCore = nullptr;
        const auto cleanup = [&] {
            if (cluster) cluster->Release();
            if (cursor) cursor->Release();
            if (attribute) attribute->Release();
            if (frameInfo) frameInfo->Release();
            if (imageSlice) imageSlice->Release();
            if (renderData) renderData->Release();
            if (engine) engine->Release();
            if (renderer) renderer->Release();
            if (controlCore) controlCore->Release();
        };
        const auto ok = udt(global, L"Microsoft::Console::Render::Cluster", 24, &cluster) &&
                        memberOffset(cluster, L"_text", 0) && memberOffset(cluster, L"_columns", 16) &&
                        udt(global, L"Microsoft::Console::Render::CursorOptions", 44, &cursor) &&
                        memberOffset(cursor, L"coordCursor", 0) && memberOffset(cursor, L"cursorType", 28) &&
                        memberOffset(cursor, L"isVisible", 40) && memberOffset(cursor, L"inViewport", 42) &&
                        udt(global, L"TextAttribute", 18, &attribute) &&
                        memberOffset(attribute, L"_attrs", 0) && memberOffset(attribute, L"_hyperlinkId", 2) &&
                        udt(global, L"Microsoft::Console::Render::RenderFrameInfo", 48, &frameInfo) &&
                        memberOffset(frameInfo, L"searchHighlights", 0) &&
                        memberOffset(frameInfo, L"searchHighlightFocused", 16) &&
                        memberOffset(frameInfo, L"selectionSpans", 24) &&
                        memberOffset(frameInfo, L"selectionBackground", 40) &&
                        udt(global, L"ImageSlice", 56, &imageSlice) &&
                        memberOffset(imageSlice, L"_revision", 0) && memberOffset(imageSlice, L"_cellSize", 8) &&
                        memberOffset(imageSlice, L"_pixelBuffer", 16) && memberOffset(imageSlice, L"_columnBegin", 40) &&
                        memberOffset(imageSlice, L"_pixelWidth", 48) &&
                        udt(global, L"Microsoft::Console::Render::IRenderData", 264, &renderData) &&
                        virtualOffset(renderData, L"LockConsole", 64) &&
                        virtualOffset(renderData, L"UnlockConsole", 72) &&
                        virtualOffset(renderData, L"GetHyperlinkUri", 152) &&
                        virtualOffset(renderData, L"GetAttributeColors", 176) &&
                        udt(global, L"Microsoft::Console::Render::IRenderEngine", 8, &engine) &&
                        virtualOffset(engine, L"PaintBufferLine", 160) &&
                        virtualOffset(engine, L"PaintCursor", 192) &&
                        virtualOffset(engine, L"UpdateDrawingBrushes", 200) &&
                        virtualOffset(engine, L"UpdateViewport", 232) &&
                        virtualOffset(engine, L"GetDirtyArea", 248) &&
                        // Lazy recovery of controls that predate injection is
                        // family-coded, never scanned: ControlCore::_renderer is
                        // an 8-byte MSVC unique_ptr, Renderer::_pData is the
                        // already-adjusted IRenderData pointer, and owner is read
                        // from its exact uint64_t member.
                        udt(global, L"Microsoft::Console::Render::Renderer", 488, &renderer) &&
                        memberOffsetAndLength(renderer, L"_pData", 24, 8) &&
                        udt(global, L"winrt::Microsoft::Terminal::Control::implementation::ControlCore", 1304, &controlCore) &&
                        memberOffsetAndLength(controlCore, L"_owningHwnd", 896, 8) &&
                        memberOffsetAndLength(controlCore, L"_renderer", 1192, 8);
        cleanup();
        if (!ok) error = "wt_1_24 renderer type layout/vtable contract mismatch";
        return ok;
    }

    bool collectSymbols(const std::filesystem::path& modulePath,
                        const std::filesystem::path& pdbPath,
                        const GUID& expectedGuid,
                        const std::uint32_t expectedAge,
                        const bool requireWt124Abi,
                        std::string& abiError,
                        std::vector<Candidate>& symbols)
    {
        const auto initialized = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
        if (FAILED(initialized) && initialized != RPC_E_CHANGED_MODE)
        {
            return false;
        }
        IDiaDataSource* source = nullptr;
        IDiaSession* session = nullptr;
        IDiaSymbol* global = nullptr;
        IDiaEnumSymbols* functions = nullptr;
        const auto cleanup = [&] {
            if (functions) functions->Release();
            if (global) global->Release();
            if (session) session->Release();
            if (source) source->Release();
            if (SUCCEEDED(initialized)) CoUninitialize();
        };
        if (FAILED(CoCreateInstance(CLSID_DiaSource, nullptr, CLSCTX_INPROC_SERVER,
                                    __uuidof(IDiaDataSource), reinterpret_cast<void**>(&source))))
        {
            cleanup();
            return false;
        }
        HRESULT loaded = E_FAIL;
        if (!pdbPath.empty())
        {
            loaded = source->loadDataFromPdb(pdbPath.c_str());
        }
        else
        {
            wchar_t* searchPath = nullptr;
            _wdupenv_s(&searchPath, nullptr, L"_NT_SYMBOL_PATH");
            loaded = source->loadDataForExe(modulePath.c_str(), searchPath, nullptr);
            std::free(searchPath);
        }
        if (FAILED(loaded) || FAILED(source->openSession(&session)) ||
            FAILED(session->get_globalScope(&global)))
        {
            cleanup();
            return false;
        }
        GUID actualGuid{};
        DWORD actualAge = 0;
        if (FAILED(global->get_guid(&actualGuid)) || FAILED(global->get_age(&actualAge)) ||
            std::memcmp(&actualGuid, &expectedGuid, sizeof(GUID)) != 0 || actualAge != expectedAge)
        {
            cleanup();
            return false;
        }
        if (requireWt124Abi && !validateWt124Abi(global, abiError))
        {
            cleanup();
            return false;
        }
        if (FAILED(global->findChildren(SymTagFunction, nullptr, nsNone, &functions)))
        {
            cleanup();
            return false;
        }
        IDiaSymbol* symbol = nullptr;
        ULONG fetched = 0;
        while (functions->Next(1, &symbol, &fetched) == S_OK && fetched == 1)
        {
            BSTR raw = nullptr;
            BSTR pretty = nullptr;
            DWORD rva = 0;
            symbol->get_name(&raw);
            symbol->get_undecoratedName(&pretty);
            symbol->get_relativeVirtualAddress(&rva);
            DWORD parameterCount = UINT32_MAX;
            IDiaSymbol* type = nullptr;
            IDiaEnumSymbols* arguments = nullptr;
            if (SUCCEEDED(symbol->get_type(&type)) && type &&
                SUCCEEDED(type->findChildren(SymTagFunctionArgType, nullptr, nsNone, &arguments)) && arguments)
            {
                LONG count = 0;
                if (SUCCEEDED(arguments->get_Count(&count)) && count >= 0)
                {
                    parameterCount = static_cast<DWORD>(count);
                }
            }
            const auto rawName = utf8(raw);
            const auto prettyName = pretty ? utf8(pretty) : rawName;
            if (!prettyName.empty() && rva != 0)
            {
                symbols.push_back({ prettyName, rawName, rva, parameterCount });
            }
            if (arguments) arguments->Release();
            if (type) type->Release();
            if (pretty) SysFreeString(pretty);
            if (raw) SysFreeString(raw);
            symbol->Release();
            symbol = nullptr;
        }

        // Some shipped system PDBs expose renderer methods only as public
        // symbols, without SymTagFunction/type records. Identity, full-image
        // hash, executable section, and prologue checks still pin those exact
        // addresses; their ABI remains family-coded in the adapter.
        IDiaEnumSymbols* publics = nullptr;
        if (SUCCEEDED(global->findChildren(SymTagPublicSymbol, nullptr, nsNone, &publics)) && publics)
        {
            while (publics->Next(1, &symbol, &fetched) == S_OK && fetched == 1)
            {
                BSTR raw = nullptr;
                BSTR pretty = nullptr;
                DWORD rva = 0;
                symbol->get_name(&raw);
                symbol->get_undecoratedName(&pretty);
                symbol->get_relativeVirtualAddress(&rva);
                const auto rawName = utf8(raw);
                const auto prettyName = pretty ? utf8(pretty) : rawName;
                if (!prettyName.empty() && rva != 0 &&
                    std::ranges::none_of(symbols, [&](const Candidate& existing) {
                        return existing.rva == rva && existing.name == prettyName;
                    }))
                {
                    symbols.push_back({ prettyName, rawName, rva, UINT32_MAX });
                }
                if (pretty) SysFreeString(pretty);
                if (raw) SysFreeString(raw);
                symbol->Release();
                symbol = nullptr;
            }
            publics->Release();
        }
        cleanup();
        return !symbols.empty();
    }

    std::string jsonEscape(const std::string_view value)
    {
        std::string out;
        for (const auto ch : value)
        {
            switch (ch)
            {
            case '\\': out += "\\\\"; break;
            case '"': out += "\\\""; break;
            case '\n': out += "\\n"; break;
            case '\r': out += "\\r"; break;
            case '\t': out += "\\t"; break;
            default:
                if (static_cast<unsigned char>(ch) >= 0x20) out += ch;
                break;
            }
        }
        return out;
    }

    struct CompatibilityReport
    {
        std::filesystem::path path;
        std::string module;
        std::string family;
        std::string status{ "incompatible" };
        std::string detail{ "profile generation failed; see stderr" };

        ~CompatibilityReport()
        {
            std::ofstream out{ path, std::ios::trunc };
            if (out)
            {
                out << "{\"module\":\"" << jsonEscape(module)
                    << "\",\"family\":\"" << jsonEscape(family)
                    << "\",\"status\":\"" << status
                    << "\",\"detail\":\"" << jsonEscape(detail) << "\"}\n";
            }
        }
    };

    bool containsAll(const std::string& value, const SymbolSpec& spec)
    {
        return std::ranges::all_of(spec.required, [&](const auto needle) {
                   return value.find(needle) != std::string::npos;
               }) &&
               std::ranges::none_of(spec.forbidden, [&](const auto needle) {
                   return value.find(needle) != std::string::npos;
               });
    }
}

int wmain(const int argc, wchar_t** argv)
{
    if (argc != 4 && argc != 5)
    {
        std::fwprintf(stderr, L"usage: shellglass-profile <module.dll> <family> <output.sgnp> [matching.pdb]\n");
        return 2;
    }
    const std::filesystem::path modulePath = argv[1];
    const std::wstring familyWide = argv[2];
    const std::filesystem::path outputPath = argv[3];
    const std::filesystem::path pdbPath = argc == 5 ? argv[4] : L"";
    auto reportPath = outputPath;
    reportPath += L".report.json";
    std::string familyName;
    familyName.reserve(familyWide.size());
    for (const auto ch : familyWide) familyName.push_back(static_cast<char>(ch));
    CompatibilityReport report{
        .path = reportPath,
        .module = modulePath.string(),
        .family = familyName,
    };
    // A failed re-profile must not leave yesterday's now-incompatible profile
    // looking usable at the requested output path.
    std::error_code removeError;
    std::filesystem::remove(outputPath, removeError);
    if (familyWide.empty() || familyWide.size() >= 32 ||
        !std::ranges::all_of(familyWide, [](const wchar_t ch) { return ch >= 0x20 && ch <= 0x7e; }))
    {
        std::fwprintf(stderr, L"family must be 1-31 printable ASCII characters\n");
        report.detail = "invalid ABI family name";
        return 2;
    }
    auto pe = loadPe(modulePath);
    if (!pe)
    {
        std::fwprintf(stderr, L"invalid or unsupported 64-bit PE: %ls\n", modulePath.c_str());
        report.detail = "invalid or unsupported 64-bit PE";
        return 1;
    }

    ProfileHeader header{ { 'S', 'G', 'N', 'P' }, 1, pe->nt->FileHeader.Machine,
                          pe->nt->OptionalHeader.SizeOfImage };
    if (!sha256(pe->bytes, header.moduleSha256) || !pdbIdentity(*pe, header.pdbGuid, header.pdbAge))
    {
        std::fwprintf(stderr, L"module hash or RSDS identity unavailable\n");
        report.detail = "module hash or RSDS identity unavailable";
        return 1;
    }
    for (std::size_t i = 0; i < familyWide.size(); ++i)
    {
        header.family[i] = static_cast<char>(familyWide[i]);
    }
    const std::span<const SymbolSpec> specs = familyWide.starts_with(L"conhost_") ?
                                                  std::span<const SymbolSpec>{ conhostSpecs } :
                                                  std::span<const SymbolSpec>{ wtSpecs };
    header.entryCount = static_cast<std::uint32_t>(specs.size());

    std::vector<Candidate> symbols;
    std::string abiError;
    if (!collectSymbols(modulePath,
                        pdbPath,
                        header.pdbGuid,
                        header.pdbAge,
                        familyWide.starts_with(L"wt_1_24"),
                        abiError,
                        symbols))
    {
        if (!abiError.empty())
        {
            std::fprintf(stderr, "%s\n", abiError.c_str());
            report.detail = abiError;
        }
        else
        {
            std::fprintf(stderr, "DIA PDB load/enumeration failed (configure _NT_SYMBOL_PATH)\n");
            report.detail = "DIA PDB load/enumeration or RSDS match failed";
        }
        return 1;
    }

    std::vector<ProfileEntry> entries;
    for (const auto& spec : specs)
    {
        std::vector<const Candidate*> matches;
        for (const auto& symbol : symbols)
        {
            if (containsAll(symbol.name, spec) &&
                (spec.parameterCount == UINT32_MAX || symbol.parameterCount == spec.parameterCount))
            {
                matches.push_back(&symbol);
            }
        }
        if (matches.size() != 1)
        {
            std::fprintf(stderr, "%s: expected one exact symbol, got %zu\n", spec.label, matches.size());
            for (const auto* match : matches)
            {
                std::fprintf(stderr, "  %s\n", match->name.c_str());
            }
            if (matches.empty())
            {
                for (const auto& symbol : symbols)
                {
                    if (symbol.name.find(spec.required.front()) != std::string::npos)
                    {
                        std::fprintf(stderr, "  candidate: %s [%s] params=%u\n", symbol.name.c_str(), symbol.decorated.c_str(), symbol.parameterCount);
                    }
                }
            }
            report.detail = std::string(spec.label) + ": expected exactly one matching symbol, got " + std::to_string(matches.size());
            return 1;
        }
        ProfileEntry entry{ spec.id, matches[0]->rva };
        if (!pe->executable(entry.rva, sizeof(entry.expected)))
        {
            std::fprintf(stderr, "%s: RVA is not in an executable section\n", spec.label);
            return 1;
        }
        const auto* expected = pe->rva(entry.rva, sizeof(entry.expected));
        if (!expected)
        {
            std::fprintf(stderr, "%s: prologue not file-backed\n", spec.label);
            return 1;
        }
        std::memcpy(entry.expected, expected, sizeof(entry.expected));
        entries.push_back(entry);
        std::fprintf(stdout, "%2u  RVA %08x  %s\n", entry.id, entry.rva, matches[0]->name.c_str());
    }
    std::vector<std::uint8_t> file(sizeof(header) + entries.size() * sizeof(ProfileEntry) + 32);
    std::memcpy(file.data(), &header, sizeof(header));
    std::memcpy(file.data() + sizeof(header), entries.data(), entries.size() * sizeof(ProfileEntry));
    if (!sha256(std::span{ file }.first(file.size() - 32), file.data() + file.size() - 32))
    {
        std::fprintf(stderr, "profile integrity hash failed\n");
        return 1;
    }
    std::ofstream output{ outputPath, std::ios::binary | std::ios::trunc };
    output.write(reinterpret_cast<const char*>(file.data()), static_cast<std::streamsize>(file.size()));
    if (!output)
    {
        std::fwprintf(stderr, L"writing profile failed: %ls\n", outputPath.c_str());
        report.detail = "writing profile failed";
        output.close();
        std::error_code ignored;
        std::filesystem::remove(outputPath, ignored);
        return 1;
    }
    report.status = "compatible";
    report.detail = "all required symbols, executable RVAs, and prologues matched";
    return 0;
}
