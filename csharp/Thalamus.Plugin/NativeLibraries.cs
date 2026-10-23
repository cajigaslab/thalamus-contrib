using System.Reflection;
using System.Runtime.InteropServices;

namespace Thalamus.Plugin;

/// <summary>
/// Native libraries shipped next to the plugin (e.g. device drivers a
/// package P/Invokes by file name). The OS loader searches Thalamus's
/// directory, not the plugin's, so assemblies importing them need
/// <see cref="ResolveFromPluginDirectory"/>.
/// </summary>
public static unsafe class NativeLibraries
{
    /// <summary>The directory this plugin's shared library was loaded from.</summary>
    public static string PluginDirectory { get; private set; } = "";

    internal static void Init()
    {
        var path = ModulePath((nint)(delegate* unmanaged<void>)&Anchor);
        PluginDirectory = path == null ? AppContext.BaseDirectory : Path.GetDirectoryName(path) ?? "";
    }

    /// <summary>Loads assembly's DllImports from <see cref="PluginDirectory"/> when present there.</summary>
    public static void ResolveFromPluginDirectory(Assembly assembly)
    {
        NativeLibrary.SetDllImportResolver(assembly, (name, _, _) =>
        {
            var path = Path.Combine(PluginDirectory, name);
            return NativeLibrary.TryLoad(path, out var handle) ? handle : 0;
        });
    }

    [UnmanagedCallersOnly]
    private static void Anchor() { }

    /// <summary>The file of the loaded module containing address.</summary>
    private static string? ModulePath(nint address)
    {
        if (OperatingSystem.IsWindows())
        {
            // GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT
            if (!GetModuleHandleExW(0x4 | 0x2, address, out var module))
            {
                return null;
            }
            var buffer = new char[32768];
            fixed (char* data = buffer)
            {
                var length = GetModuleFileNameW(module, data, (uint)buffer.Length);
                return length == 0 ? null : new string(data, 0, (int)length);
            }
        }

        DlInfo info;
        var found = OperatingSystem.IsMacOS() ? DlAddrMac(address, &info) : DlAddrLinux(address, &info);
        return found == 0 || info.FileName == 0 ? null : Marshal.PtrToStringUTF8(info.FileName);
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct DlInfo
    {
        public nint FileName;
        public nint FileBase;
        public nint SymbolName;
        public nint SymbolAddress;
    }

    [DllImport("kernel32", SetLastError = true)]
    private static extern bool GetModuleHandleExW(uint flags, nint address, out nint module);

    [DllImport("kernel32", SetLastError = true)]
    private static extern uint GetModuleFileNameW(nint module, char* filename, uint size);

    // libdl.so.2 exists on every glibc, including 2.34+ where dladdr moved into libc.
    [DllImport("libdl.so.2", EntryPoint = "dladdr")]
    private static extern int DlAddrLinux(nint address, DlInfo* info);

    [DllImport("/usr/lib/libSystem.B.dylib", EntryPoint = "dladdr")]
    private static extern int DlAddrMac(nint address, DlInfo* info);
}
