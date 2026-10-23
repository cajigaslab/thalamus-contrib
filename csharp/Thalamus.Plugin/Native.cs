// C ABI of Thalamus's plugin.h (rust/include/thalamus/plugin.h). Structs are
// append-only on both sides; keep field order identical to the header.
using System.Runtime.InteropServices;

namespace Thalamus.Plugin.Interop;

[StructLayout(LayoutKind.Sequential)]
internal struct CharSpan
{
    public nint Data;
    public ulong Size;
    public byte OwnsData;
}

[StructLayout(LayoutKind.Sequential)]
internal struct Span
{
    public nint Data;
    public ulong Size;
}

[StructLayout(LayoutKind.Sequential)]
internal unsafe struct ThalamusNode
{
    // Thalamus's ExtNode, passed to create2; node_ready and friends read it.
    public nint Impl;
    public delegate* unmanaged<ThalamusNode*, ulong> TimeNs;
    public AnalogVTable* Analog;
    public nint Mocap;
    public nint Image;
    public TextVTable* Text;
    // GCHandle of this plugin's NodeBox.
    public nint PluginImpl;
    public delegate* unmanaged<ThalamusNode*, nint, nint, void> Process;
    public delegate* unmanaged<ThalamusNode*, void> Predrop;
    public byte SignalsOffmain;
    // Only read by Thalamus from plugins exporting thalamus_get_node_version, which this one doesn't.
    public nint Blob;
}

[StructLayout(LayoutKind.Sequential)]
internal unsafe struct AnalogVTable
{
    public delegate* unmanaged<Span*, ThalamusNode*, int, void> Data;
    public delegate* unmanaged<Span*, ThalamusNode*, int, void> ShortData;
    public delegate* unmanaged<Span*, ThalamusNode*, int, void> IntData;
    public delegate* unmanaged<Span*, ThalamusNode*, int, void> ULongData;
    public delegate* unmanaged<ThalamusNode*, int> NumChannels;
    public delegate* unmanaged<ThalamusNode*, int, ulong> SampleIntervalNs;
    public delegate* unmanaged<ThalamusNode*, byte> HasAnalogData;
    public delegate* unmanaged<ThalamusNode*, byte> IsShortData;
    public delegate* unmanaged<ThalamusNode*, byte> IsIntData;
    public delegate* unmanaged<ThalamusNode*, byte> IsULongData;
    public delegate* unmanaged<ThalamusNode*, byte> IsTransformed;
    public delegate* unmanaged<ThalamusNode*, int, double> Scale;
    public delegate* unmanaged<ThalamusNode*, int, double> Offset;
    public delegate* unmanaged<CharSpan*, ThalamusNode*, int, void> Name;
    // Fields after Name are counted by thalamus_get_analog_node_version.
    public delegate* unmanaged<Span*, ThalamusNode*, void> Buffer;
    public delegate* unmanaged<ThalamusNode*, int> Encoding;
    public delegate* unmanaged<ThalamusNode*, int, int> Format;
    public delegate* unmanaged<ThalamusNode*, ulong> EncodedCount;
    public delegate* unmanaged<ThalamusNode*, byte> ChannelsChanged;
    public delegate* unmanaged<ThalamusNode*, ulong> RemoteTimeNs;
}

[StructLayout(LayoutKind.Sequential)]
internal unsafe struct TextVTable
{
    public delegate* unmanaged<CharSpan*, ThalamusNode*, void> Text;
    public delegate* unmanaged<ThalamusNode*, byte> HasTextData;
    // Fields after HasTextData are counted by thalamus_get_text_node_version.
    public delegate* unmanaged<ThalamusNode*, ulong> RemoteTimeNs;
}

[StructLayout(LayoutKind.Sequential)]
internal unsafe struct NodeFactory
{
    public CharSpan Type;
    public delegate* unmanaged<NodeFactory*, nint, nint, nint, ThalamusNode*> Create;
    public delegate* unmanaged<NodeFactory*, ThalamusNode*, void> Destroy;
    public delegate* unmanaged<NodeFactory*, byte> Prepare;
    public delegate* unmanaged<NodeFactory*, void> Cleanup;
    // GCHandle of the NodeType.
    public nint PluginImpl;
    public delegate* unmanaged<NodeFactory*, nint, nint, nint, nint, ThalamusNode*> Create2;
}

/// <summary>
/// This plugin's copy of the ThalamusAPI table. Only the first <c>version</c>
/// slots of Thalamus's table are copied (like Rust's copy_from_host), so slots
/// the running Thalamus lacks read as zero; check <see cref="Has"/> before
/// calling a slot newer than the oldest supported Thalamus.
/// </summary>
internal static unsafe class Api
{
    // Slot numbers from the comments in plugin.h's ThalamusAPI.
    public const int StateIsDict = 1;
    public const int StateIsList = 2;
    public const int StateIsString = 3;
    public const int StateIsInt = 4;
    public const int StateIsFloat = 5;
    public const int StateIsNull = 6;
    public const int StateIsBool = 7;
    public const int StateGetString = 8;
    public const int StateGetInt = 9;
    public const int StateGetFloat = 10;
    public const int StateGetBool = 11;
    public const int StateGetAtName = 12;
    public const int StateGetAtIndex = 13;
    public const int StateDecRef = 14;
    public const int StateIncRef = 15;
    public const int StateRecursiveChangeConnect = 16;
    public const int StateRecursiveChangeDisconnect = 17;
    public const int NodeReady = 23;
    public const int TimeNs = 24;
    public const int StateSetAtNameState = 27;
    public const int StateSetAtNameString = 28;
    public const int StateSetAtNameInt = 29;
    public const int StateSetAtNameFloat = 30;
    public const int StateSetAtNameNull = 31;
    public const int StateSetAtNameBool = 32;
    public const int StateSetAtIndexState = 33;
    public const int StateSetAtIndexString = 34;
    public const int StateSetAtIndexInt = 35;
    public const int StateSetAtIndexFloat = 36;
    public const int StateSetAtIndexNull = 37;
    public const int StateSetAtIndexBool = 38;
    public const int IoContextPost = 39;
    public const int CharSpanRelease = 56;
    public const int JsonToString = 58;
    public const int JsonFromString = 59;
    public const int RequestRespond = 60;
    public const int JsonDecRef = 62;
    public const int StateIterCreate = 73;
    public const int StateIterNext = 74;
    public const int StateIterKey = 75;
    public const int StateIterValue = 76;
    public const int StateIterDestroy = 77;
    public const int StateRecapWith = 79;
    public const int NodePredropReady = 84;
    public const int DialogShow = 137;
    public const int Log = 153;

    private const int SlotCount = 153;

    private static nint* slots;

    public static int Version { get; private set; }

    public static void Init(nint host)
    {
        Version = *(int*)host;
        var count = Math.Clamp(Version, 0, SlotCount);
        slots = (nint*)NativeMemory.AllocZeroed((nuint)(SlotCount + 1), (nuint)sizeof(nint));
        // The int32 version is padded to pointer size, so slot i is pointer i.
        for (var i = 1; i <= count; ++i)
        {
            slots[i] = ((nint*)host)[i];
        }
    }

    public static bool Has(int slot) => slots != null && slots[slot] != 0;

    public static nint Slot(int slot)
    {
        var result = slots == null ? 0 : slots[slot];
        if (result == 0)
        {
            throw new NotSupportedException($"ThalamusAPI slot {slot} isn't provided by this Thalamus (API version {Version})");
        }
        return result;
    }

    public static string ToManaged(CharSpan span)
    {
        return span.Data == 0 ? "" : System.Text.Encoding.UTF8.GetString((byte*)span.Data, checked((int)span.Size));
    }
}
