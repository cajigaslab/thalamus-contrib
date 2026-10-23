// The C glue between Thalamus and C# nodes, the counterpart of rust/src/ffi.rs:
// node factories, the callbacks that expose nodes to Thalamus and the
// thalamus_get_* exports.
using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json.Nodes;
using Thalamus.Plugin.Interop;

namespace Thalamus.Plugin;

public static unsafe class Host
{
    /// <summary>
    /// Implements thalamus_get_node_factories for the plugin's node types.
    /// Call it from the plugin's own [UnmanagedCallersOnly] export.
    /// </summary>
    public static nint GetNodeFactories(nint api, IReadOnlyList<NodeType> types)
    {
        try
        {
            Api.Init(api);
            MainThread.Init();
            NativeLibraries.Init();
            var factories = (NodeFactory**)NativeMemory.AllocZeroed((nuint)(types.Count + 1), (nuint)sizeof(nint));
            for (var i = 0; i < types.Count; ++i)
            {
                var factory = (NodeFactory*)NativeMemory.AllocZeroed((nuint)sizeof(NodeFactory));
                factory->Type = Utf8.Alloc(types[i].Name);
                factory->Create = &Create;
                factory->Destroy = &Destroy;
                factory->Prepare = &Prepare;
                factory->Cleanup = &Cleanup;
                factory->PluginImpl = GCHandle.ToIntPtr(GCHandle.Alloc(types[i]));
                factory->Create2 = &Create2;
                factories[i] = factory;
            }
            // Thalamus keeps the factories until it unloads the plugin; they're never freed.
            return (nint)factories;
        }
        catch (Exception ex)
        {
            ThalamusHost.Report(ex, "thalamus_get_node_factories");
            var empty = (nint*)NativeMemory.AllocZeroed((nuint)sizeof(nint));
            return (nint)empty;
        }
    }

    /// <summary>The number of fields after ThalamusNodeFactory::plugin_impl this plugin fills: create2.</summary>
    [UnmanagedCallersOnly(EntryPoint = "thalamus_get_node_factory_version")]
    private static int NodeFactoryVersion() => 1;

    /// <summary>
    /// The number of fields after ThalamusAnalogNode::name this plugin fills:
    /// buffer, encoding, format, encoded_count, channels_changed and remote_time_ns.
    /// </summary>
    [UnmanagedCallersOnly(EntryPoint = "thalamus_get_analog_node_version")]
    private static int AnalogNodeVersion() => 6;

    /// <summary>The number of fields after ThalamusTextNode::has_text_data this plugin fills: remote_time_ns.</summary>
    [UnmanagedCallersOnly(EntryPoint = "thalamus_get_text_node_version")]
    private static int TextNodeVersion() => 1;

    [UnmanagedCallersOnly(EntryPoint = "thalamus_teardown")]
    private static void Teardown() => MainThread.Stop();

    /// <summary>A node and the native memory Thalamus reads its data from.</summary>
    private sealed class NodeBox(NodeContext context)
    {
        public NodeContext Context { get; } = context;
        public Node? Node { get; set; }
        public IAnalogNode? Analog => Node as IAnalogNode;
        public ITextNode? Text => Node as ITextNode;
        // Per channel sample copies and UTF-8 names, valid until the next message.
        public List<(nint Data, int Capacity)> Samples { get; } = [];
        public Dictionary<int, (string Text, CharSpan Span)> Names { get; } = [];
        public (string Text, CharSpan Span) TextData { get; set; } = ("", default);
    }

    private static NodeBox Box(ThalamusNode* node) => (NodeBox)GCHandle.FromIntPtr(node->PluginImpl).Target!;

    private static NodeType Type(NodeFactory* factory) => (NodeType)GCHandle.FromIntPtr(factory->PluginImpl).Target!;

    [UnmanagedCallersOnly]
    private static byte Prepare(NodeFactory* factory)
    {
        MainThread.Enter();
        try
        {
            return Type(factory).PrepareType() ? (byte)1 : (byte)0;
        }
        catch (Exception ex)
        {
            ThalamusHost.Report(ex, Type(factory).Name);
            return 0;
        }
    }

    [UnmanagedCallersOnly]
    private static void Cleanup(NodeFactory* factory)
    {
        MainThread.Enter();
        try
        {
            Type(factory).CleanupType();
        }
        catch (Exception ex)
        {
            ThalamusHost.Report(ex, Type(factory).Name);
        }
    }

    [UnmanagedCallersOnly]
    private static ThalamusNode* Create(NodeFactory* factory, nint state, nint ioContext, nint graph) => CreateNode(factory, state, 0);

    [UnmanagedCallersOnly]
    private static ThalamusNode* Create2(NodeFactory* factory, nint state, nint ioContext, nint graph, nint impl) => CreateNode(factory, state, impl);

    private static ThalamusNode* CreateNode(NodeFactory* factory, nint state, nint impl)
    {
        MainThread.Enter();
        var type = Type(factory);
        var native = (ThalamusNode*)NativeMemory.AllocZeroed((nuint)sizeof(ThalamusNode));
        native->Impl = impl;
        native->TimeNs = &TimeNs;
        native->Process = &Process;
        native->Predrop = &Predrop;

        var context = new NodeContext(native, new State(state));
        var box = new NodeBox(context);
        native->PluginImpl = GCHandle.ToIntPtr(GCHandle.Alloc(box));
        try
        {
            box.Node = type.CreateNode(context);
        }
        catch (Exception ex)
        {
            // Thalamus needs a node either way; this one does nothing.
            ThalamusHost.Report(ex, $"{type.Name} constructor");
            ThalamusHost.ShowDialog($"{type.Name} Error", $"Failed to create node: {ex.Message}", DialogType.Error);
            box.Node = new FailedNode(context);
        }
        context.Constructed = true;

        if (box.Analog != null)
        {
            var analog = (AnalogVTable*)NativeMemory.AllocZeroed((nuint)sizeof(AnalogVTable));
            analog->Data = &AnalogData;
            analog->ShortData = &AnalogEmpty;
            analog->IntData = &AnalogEmpty;
            analog->ULongData = &AnalogEmpty;
            analog->NumChannels = &AnalogNumChannels;
            analog->SampleIntervalNs = &AnalogSampleIntervalNs;
            analog->HasAnalogData = &AnalogHasAnalogData;
            analog->IsShortData = &False;
            analog->IsIntData = &False;
            analog->IsULongData = &False;
            analog->IsTransformed = &False;
            analog->Scale = &AnalogScale;
            analog->Offset = &AnalogOffset;
            analog->Name = &AnalogName;
            analog->Buffer = &AnalogBuffer;
            analog->Encoding = &AnalogEncoding;
            analog->Format = &AnalogFormat;
            analog->EncodedCount = &Zero;
            analog->ChannelsChanged = &AnalogChannelsChanged;
            analog->RemoteTimeNs = &Zero;
            native->Analog = analog;
        }
        if (box.Text != null)
        {
            var text = (TextVTable*)NativeMemory.AllocZeroed((nuint)sizeof(TextVTable));
            text->Text = &TextText;
            text->HasTextData = &TextHasTextData;
            text->RemoteTimeNs = &Zero;
            native->Text = text;
        }
        return native;
    }

    [UnmanagedCallersOnly]
    private static void Destroy(NodeFactory* factory, ThalamusNode* native)
    {
        MainThread.Enter();
        var handle = GCHandle.FromIntPtr(native->PluginImpl);
        var box = (NodeBox)handle.Target!;
        // Ready calls from work still pending for this node become no-ops.
        box.Context.Destroyed = true;
        try
        {
            box.Node?.Dispose();
        }
        catch (Exception ex)
        {
            ThalamusHost.Report(ex, $"{Type(factory).Name} Dispose");
        }
        foreach (var (data, _) in box.Samples)
        {
            NativeMemory.Free((void*)data);
        }
        foreach (var (_, span) in box.Names.Values)
        {
            NativeMemory.Free((void*)span.Data);
        }
        NativeMemory.Free((void*)box.TextData.Span.Data);
        NativeMemory.Free(native->Analog);
        NativeMemory.Free(native->Text);
        handle.Free();
        NativeMemory.Free(native);
    }

    [UnmanagedCallersOnly]
    private static ulong TimeNs(ThalamusNode* node) => (ulong)Box(node).Context.TimeNs;

    [UnmanagedCallersOnly]
    private static void Process(ThalamusNode* native, nint handle, nint json)
    {
        MainThread.Enter();
        // json is only valid during this call.
        CharSpan span;
        ((delegate* unmanaged<CharSpan*, nint, void>)Api.Slot(Api.JsonToString))(&span, json);
        var text = Api.ToManaged(span);
        ((delegate* unmanaged<CharSpan*, void>)Api.Slot(Api.CharSpanRelease))(&span);
        _ = NodeTasks.Respond(Box(native).Node!, handle, text);
    }

    internal static void SendResponse(nint handle, string response)
    {
        var bytes = Encoding.UTF8.GetBytes(response);
        fixed (byte* data = bytes)
        {
            var span = new CharSpan { Data = (nint)data, Size = (ulong)bytes.Length };
            var json = ((delegate* unmanaged<CharSpan*, nint>)Api.Slot(Api.JsonFromString))(&span);
            ((delegate* unmanaged<nint, nint, void>)Api.Slot(Api.RequestRespond))(handle, json);
            ((delegate* unmanaged<nint, void>)Api.Slot(Api.JsonDecRef))(json);
        }
    }

    [UnmanagedCallersOnly]
    private static void Predrop(ThalamusNode* native)
    {
        MainThread.Enter();
        _ = NodeTasks.Predrop(Box(native).Node!, (nint)native);
    }

    internal static void PredropReady(nint native) =>
        ((delegate* unmanaged<nint, void>)Api.Slot(Api.NodePredropReady))(native);

    [UnmanagedCallersOnly]
    private static void AnalogData(Span* output, ThalamusNode* node, int channel)
    {
        var box = Box(node);
        try
        {
            var samples = box.Analog!.Data(channel);
            while (box.Samples.Count <= channel)
            {
                box.Samples.Add((0, 0));
            }
            var (data, capacity) = box.Samples[channel];
            if (capacity < samples.Length)
            {
                NativeMemory.Free((void*)data);
                capacity = samples.Length;
                data = (nint)NativeMemory.Alloc((nuint)capacity, sizeof(double));
                box.Samples[channel] = (data, capacity);
            }
            samples.CopyTo(new Span<double>((void*)data, samples.Length));
            *output = new Span { Data = data, Size = (ulong)samples.Length };
        }
        catch (Exception ex)
        {
            ThalamusHost.Report(ex, "IAnalogNode.Data");
            *output = default;
        }
    }

    [UnmanagedCallersOnly]
    private static void AnalogEmpty(Span* output, ThalamusNode* node, int channel) => *output = default;

    [UnmanagedCallersOnly]
    private static int AnalogNumChannels(ThalamusNode* node) => Guard(() => Box(node).Analog!.NumChannels(), 0);

    [UnmanagedCallersOnly]
    private static ulong AnalogSampleIntervalNs(ThalamusNode* node, int channel) =>
        (ulong)Guard(() => Box(node).Analog!.SampleIntervalNs(channel), 0L);

    [UnmanagedCallersOnly]
    private static byte AnalogHasAnalogData(ThalamusNode* node) => Guard(() => Box(node).Analog!.HasAnalogData(), false) ? (byte)1 : (byte)0;

    [UnmanagedCallersOnly]
    private static byte AnalogChannelsChanged(ThalamusNode* node) => Guard(() => Box(node).Analog!.ChannelsChanged(), false) ? (byte)1 : (byte)0;

    [UnmanagedCallersOnly]
    private static void AnalogName(CharSpan* output, ThalamusNode* node, int channel)
    {
        var box = Box(node);
        var name = Guard(() => box.Analog!.Name(channel), "");
        if (!box.Names.TryGetValue(channel, out var cached) || cached.Text != name)
        {
            if (cached.Span.Data != 0)
            {
                NativeMemory.Free((void*)cached.Span.Data);
            }
            cached = (name, Utf8.Alloc(name));
            box.Names[channel] = cached;
        }
        *output = cached.Span;
    }

    [UnmanagedCallersOnly]
    private static double AnalogScale(ThalamusNode* node, int channel) => 1.0;

    [UnmanagedCallersOnly]
    private static double AnalogOffset(ThalamusNode* node, int channel) => 0.0;

    [UnmanagedCallersOnly]
    private static void AnalogBuffer(Span* output, ThalamusNode* node) => *output = default;

    // ThalamusAnalogEncoding_None
    [UnmanagedCallersOnly]
    private static int AnalogEncoding(ThalamusNode* node) => 0;

    // ThalamusAnalogFormat_Double
    [UnmanagedCallersOnly]
    private static int AnalogFormat(ThalamusNode* node, int channel) => 0;

    [UnmanagedCallersOnly]
    private static void TextText(CharSpan* output, ThalamusNode* node)
    {
        var box = Box(node);
        var text = Guard(() => box.Text!.Text(), "");
        if (box.TextData.Text != text || box.TextData.Span.Data == 0)
        {
            NativeMemory.Free((void*)box.TextData.Span.Data);
            box.TextData = (text, Utf8.Alloc(text));
        }
        *output = box.TextData.Span;
    }

    [UnmanagedCallersOnly]
    private static byte TextHasTextData(ThalamusNode* node) => Guard(() => Box(node).Text!.HasTextData(), false) ? (byte)1 : (byte)0;

    [UnmanagedCallersOnly]
    private static byte False(ThalamusNode* node) => 0;

    [UnmanagedCallersOnly]
    private static ulong Zero(ThalamusNode* node) => 0;

    private static T Guard<T>(Func<T> getter, T fallback)
    {
        try
        {
            return getter();
        }
        catch (Exception ex)
        {
            ThalamusHost.Report(ex, "node data callback");
            return fallback;
        }
    }

    private sealed class FailedNode(NodeContext context) : Node(context);
}

/// <summary>Host's async callbacks; await isn't allowed in Host's unsafe context.</summary>
internal static class NodeTasks
{
    public static async Task Respond(Node node, nint handle, string requestText)
    {
        JsonNode? response;
        try
        {
            response = await node.Process(JsonNode.Parse(requestText));
        }
        catch (Exception ex)
        {
            ThalamusHost.Report(ex, "Process");
            response = new JsonObject { ["error"] = ex.Message };
        }
        // The await resumed on the main thread, where Thalamus expects the response.
        Host.SendResponse(handle, response?.ToJsonString() ?? "null");
    }

    public static async Task Predrop(Node node, nint native)
    {
        try
        {
            await node.Predrop();
        }
        catch (Exception ex)
        {
            ThalamusHost.Report(ex, "Predrop");
        }
        Host.PredropReady(native);
    }
}

internal static unsafe class Utf8
{
    /// <summary>A NUL terminated native copy of text; free Data with NativeMemory.Free.</summary>
    public static CharSpan Alloc(string text)
    {
        var count = Encoding.UTF8.GetByteCount(text);
        var data = (byte*)NativeMemory.Alloc((nuint)count + 1);
        Encoding.UTF8.GetBytes(text, new Span<byte>(data, count));
        data[count] = 0;
        return new CharSpan { Data = (nint)data, Size = (ulong)count };
    }
}
