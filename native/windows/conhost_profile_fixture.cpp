// Deterministic DIA fixture for the conhost public-symbol profile family.
#include <windows.h>
#include <intrin.h>

namespace Microsoft::Console::Render
{
    class IRenderEngine {};

    class __declspec(dllexport) Renderer
    {
    public:
        __declspec(noinline) virtual void AddRenderEngine(IRenderEngine* const engine);
        __declspec(noinline) virtual void TriggerRedrawAll();
        __declspec(noinline) virtual long PaintFrame();
    };

    void Renderer::AddRenderEngine(IRenderEngine* const engine)
    {
        (void)engine;
        _ReadWriteBarrier();
    }
    void Renderer::TriggerRedrawAll()
    {
        _ReadWriteBarrier();
    }
    long Renderer::PaintFrame()
    {
        _ReadWriteBarrier();
        return S_OK;
    }
}

BOOL WINAPI DllMain(HINSTANCE, DWORD, void*) { return TRUE; }
