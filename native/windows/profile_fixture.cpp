// Deliberately tiny private-ABI symbol fixture for exercising profile generation.
// It is never injected and ships no terminal behavior.
#include <intrin.h>
namespace winrt::Microsoft::Terminal::Control::implementation
{
    class __declspec(dllexport) ControlCore
    {
    public:
        virtual ~ControlCore();
        bool Initialize(float, float, float);
        void _focusChanged(bool);
        void OwningHwnd(unsigned long long);
    };

    ControlCore::~ControlCore() = default;
    __declspec(noinline) bool ControlCore::Initialize(float width, float height, float scale)
    {
        return width >= 0.0f && height >= 0.0f && scale > 0.0f;
    }
    __declspec(noinline) void ControlCore::_focusChanged(bool focused)
    {
        if (focused)
        {
            _ReadWriteBarrier();
        }
    }
    __declspec(noinline) void ControlCore::OwningHwnd(unsigned long long owner)
    {
        if (owner != 0)
        {
            _ReadWriteBarrier();
        }
    }
}

class TextAttribute;
namespace Microsoft::Console::Render
{
    class IRenderEngine;
    class __declspec(dllexport) RenderSettings
    {
    public:
        __declspec(noinline) unsigned long GetAttributeUnderlineColor(const TextAttribute&) const noexcept;
    };
    unsigned long RenderSettings::GetAttributeUnderlineColor(const TextAttribute&) const noexcept
    {
        _ReadWriteBarrier();
        return 0;
    }
    class __declspec(dllexport) Renderer
    {
    public:
        __declspec(noinline) void AddRenderEngine(IRenderEngine* engine);
        __declspec(noinline) void RemoveRenderEngine(IRenderEngine* engine);
        __declspec(noinline) void TriggerRedrawAll(bool backgroundChanged, bool frameChanged);
    };

    void Renderer::AddRenderEngine(IRenderEngine* engine) { (void)engine; _ReadWriteBarrier(); }
    void Renderer::RemoveRenderEngine(IRenderEngine* engine) { (void)engine; _ReadWriteBarrier(); }
    void Renderer::TriggerRedrawAll(bool backgroundChanged, bool frameChanged)
    {
        if (backgroundChanged || frameChanged)
        {
            _ReadWriteBarrier();
        }
    }
}
